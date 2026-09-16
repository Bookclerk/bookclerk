//! Mediated TCP sockets for native guests (Workers `connect()` equivalent).
//!
//! Native plugins must not call `socket(AF_INET)` / `connect` themselves. The
//! nested jail denies ambient internet; this module speaks HTTP CONNECT to the
//! host socket proxy (`BOOKCLERK_SOCKET_PROXY`) which applies the same
//! [`bookclerk_plugin_manifest::EgressPolicy`] as workerd `fetch()`/`connect()`.
//!
//! On Linux the launcher sets `BOOKCLERK_SOCKET_PROXY=/proc/self/fd/{n}/sockets.sock`
//! (nested Landlock ABI 6 scopes abstract Unix sockets out of the guest domain).
//! `abstract:{name}` is still accepted for tests and older launchers.
//! On Windows the launcher sets `BOOKCLERK_SOCKET_PROXY=\\.\pipe\bc-s-{hex}` with a
//! Package-SID DACL so the nested AppContainer guest can CONNECT.

#![allow(clippy::missing_docs_in_private_items)]

use crate::error::{Result, SdkError};

#[cfg(all(test, any(unix, feature = "http")))]
use std::sync::Mutex;

/// Env var set by `bookclerk-workerd` for native-behind-workerd guests.
pub const SOCKET_PROXY_ENV: &str = "BOOKCLERK_SOCKET_PROXY";

/// Host sets this to `1` so the native backend is nested under `NetPolicy::Deny`.
///
/// SDK HTTP and native TCP fail closed when this is set and
/// [`SOCKET_PROXY_ENV`] is unset.
pub const NESTED_NATIVE_JAIL_ENV: &str = "BOOKCLERK_NESTED_NATIVE_JAIL";

/// True when the host nested the native backend under `NetPolicy::Deny`.
#[must_use]
pub fn nested_native_jail_requested() -> bool {
    std::env::var(NESTED_NATIVE_JAIL_ENV).as_deref() == Ok("1")
}

/// Process-wide lock for tests that mutate [`SOCKET_PROXY_ENV`].
///
/// Unix net tests and `http` tests take this lock. Windows without `http` has
/// no SOCKET_PROXY env mutation in this crate.
#[cfg(all(test, any(unix, feature = "http")))]
pub(crate) static SOCKET_PROXY_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Prefix for [`SOCKET_PROXY_ENV`] when the proxy is a Linux abstract socket.
pub const SOCKET_PROXY_ABSTRACT_PREFIX: &str = "abstract:";

/// Byte stream to the host CONNECT proxy (past the HTTP handshake).
#[cfg(unix)]
pub type ProxyStream = tokio::net::UnixStream;
/// Byte stream to the host CONNECT proxy (past the HTTP handshake).
#[cfg(windows)]
pub type ProxyStream = tokio::net::windows::named_pipe::NamedPipeClient;

/// Destination for [`connect`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketAddress {
    /// Hostname or IP literal.
    pub hostname: String,
    /// Destination port.
    pub port: u16,
}

/// Cloudflare-compatible `secureTransport` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SecureTransport {
    /// Plain TCP (default).
    #[default]
    Off,
    /// TLS from the first byte. Authors wrap the stream with rustls after CONNECT.
    On,
    /// Plain TCP until [`PluginSocket::start_tls`].
    StartTls,
}

/// Options for [`connect`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConnectOptions {
    /// TLS mode.
    pub secure_transport: SecureTransport,
    /// Accepted for API compatibility; half-close is always allowed.
    pub allow_half_open: bool,
}

/// Bidirectional stream returned by [`connect`].
pub struct PluginSocket {
    #[cfg(any(unix, windows))]
    stream: ProxyStream,
    secure: SecureTransport,
    started_tls: bool,
}

impl PluginSocket {
    /// Close both directions.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when shutdown fails.
    pub async fn close(self) -> Result<()> {
        #[cfg(any(unix, windows))]
        {
            use tokio::io::AsyncWriteExt;
            let mut stream = self.stream;
            stream.shutdown().await?;
            Ok(())
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = self;
            Err(SdkError::message("native sockets require Unix or Windows"))
        }
    }

    /// Borrow the proxied stream (past CONNECT).
    #[cfg(any(unix, windows))]
    #[must_use]
    pub fn stream(&mut self) -> &mut ProxyStream {
        &mut self.stream
    }

    /// Split into owned reader/writer halves.
    #[cfg(unix)]
    #[must_use]
    pub fn into_split(
        self,
    ) -> (
        tokio::net::unix::OwnedReadHalf,
        tokio::net::unix::OwnedWriteHalf,
    ) {
        self.stream.into_split()
    }

    /// Consumes the socket, returning the CONNECT-established proxy stream.
    #[cfg(any(unix, windows))]
    #[must_use]
    pub fn into_stream(self) -> ProxyStream {
        self.stream
    }

    /// Consumes the socket, returning the CONNECT-established Unix stream.
    #[cfg(unix)]
    #[must_use]
    pub fn into_unix_stream(self) -> tokio::net::UnixStream {
        self.into_stream()
    }

    /// Start TLS on a `starttls` socket.
    ///
    /// The proxy is byte-transparent, so TLS is end-to-end. Wrap
    /// [`Self::into_split`] with `tokio-rustls` (same pattern as workerd
    /// `socket.startTls()` after `secureTransport: "starttls"`).
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when `secureTransport` was not `starttls`.
    pub async fn start_tls(&mut self) -> Result<()> {
        if self.secure != SecureTransport::StartTls {
            return Err(SdkError::message(
                "start_tls requires connect(..., SecureTransport::StartTls)",
            ));
        }
        if self.started_tls {
            return Err(SdkError::message("start_tls has already been called"));
        }
        self.started_tls = true;
        Err(SdkError::message(
            "native startTls: wrap PluginSocket::into_split() with tokio-rustls (proxy is byte-transparent)",
        ))
    }
}

/// Open a TCP connection through the Bookclerk socket proxy.
///
/// Mirrors `import { connect } from "cloudflare:sockets"`.
///
/// # Errors
///
/// Returns [`SdkError`] when the proxy env is missing, CONNECT is denied, or
/// I/O fails.
pub async fn connect(address: SocketAddress, options: ConnectOptions) -> Result<PluginSocket> {
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (address, options);
        return Err(SdkError::message(
            "native TCP sockets require Unix or Windows in this Bookclerk build",
        ));
    }
    #[cfg(any(unix, windows))]
    {
        use tokio::io::AsyncWriteExt;

        let spec = std::env::var(SOCKET_PROXY_ENV).map_err(|_| {
            SdkError::message(
                "BOOKCLERK_SOCKET_PROXY is unset; native TCP requires native-behind-workerd",
            )
        })?;
        let mut stream = connect_proxy(&spec).await?;
        let host = if address.hostname.contains(':') && !address.hostname.starts_with('[') {
            format!("[{}]:{}", address.hostname, address.port)
        } else {
            format!("{}:{}", address.hostname, address.port)
        };
        let req = format!("CONNECT {host} HTTP/1.1\r\nHost: {host}\r\n\r\n");
        stream.write_all(req.as_bytes()).await?;
        stream.flush().await?;
        let header = read_http_head(&mut stream).await?;
        let status_line = header.lines().next().unwrap_or("");
        if !status_line.contains("200") {
            return Err(SdkError::message(format!(
                "socket proxy refused {host}: {status_line}"
            )));
        }
        if options.secure_transport == SecureTransport::On {
            return Err(SdkError::message(
                "SecureTransport::On: CONNECT succeeded; use bookclerk_plugin_sdk::http::connect_tls \
(or wrap PluginSocket::into_stream with tokio-rustls)",
            ));
        }
        Ok(PluginSocket {
            stream,
            secure: options.secure_transport,
            started_tls: false,
        })
    }
}

#[cfg(unix)]
/// Connects to the host socket proxy (pathname or Linux `abstract:` name).
///
/// # Errors
///
/// Returns an I/O error when the proxy cannot be reached, or a message when
/// `abstract:` is used off Linux.
async fn connect_proxy(spec: &str) -> Result<ProxyStream> {
    use tokio::net::UnixStream;
    if let Some(name) = spec.strip_prefix(SOCKET_PROXY_ABSTRACT_PREFIX) {
        return connect_abstract(name).await;
    }
    Ok(UnixStream::connect(spec).await?)
}

#[cfg(windows)]
/// Connects to the host SOCKET_PROXY named pipe.
///
/// # Errors
///
/// Returns an I/O error when `CreateFile` on the pipe fails.
async fn connect_proxy(spec: &str) -> Result<ProxyStream> {
    use tokio::net::windows::named_pipe::ClientOptions;
    let name = spec.strip_prefix("pipe:").unwrap_or(spec);
    Ok(ClientOptions::new().open(name)?)
}

#[cfg(all(unix, target_os = "linux"))]
/// Connects to a Linux abstract-namespace Unix socket.
///
/// # Errors
///
/// Returns an I/O error when the name is invalid or `connect(2)` fails.
async fn connect_abstract(name: &str) -> Result<tokio::net::UnixStream> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixStream as StdUnixStream};
    use tokio::net::UnixStream;

    let addr = SocketAddr::from_abstract_name(name.as_bytes())?;
    let std_stream = StdUnixStream::connect_addr(&addr)?;
    std_stream.set_nonblocking(true)?;
    Ok(UnixStream::from_std(std_stream)?)
}

#[cfg(all(unix, not(target_os = "linux")))]
/// Linux abstract sockets are not available on this target.
///
/// # Errors
///
/// Always returns a message error.
async fn connect_abstract(_name: &str) -> Result<tokio::net::UnixStream> {
    Err(SdkError::message(
        "BOOKCLERK_SOCKET_PROXY=abstract:… is Linux-only",
    ))
}

#[cfg(any(unix, windows))]
/// Reads the CONNECT response header from the socket proxy.
///
/// # Errors
///
/// Returns an error when the stream ends early, the handshake exceeds 8 KiB,
/// or the proxy returns a non-success status.
async fn read_http_head<S>(stream: &mut S) -> Result<String>
where
    S: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let mut tmp = [0_u8; 1];
    loop {
        stream.read_exact(&mut tmp).await?;
        buf.push(tmp[0]);
        if buf.len() >= 4 && buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > 8192 {
            return Err(SdkError::message("socket proxy handshake too large"));
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(all(test, unix))]
#[allow(clippy::missing_panics_doc, clippy::await_holding_lock)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn connect_through_fake_proxy_and_403() {
        let _guard = SOCKET_PROXY_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().unwrap();
        let ok_path = dir.path().join("ok.sock");
        let deny_path = dir.path().join("deny.sock");
        let ok_listener = tokio::net::UnixListener::bind(&ok_path).unwrap();
        let deny_listener = tokio::net::UnixListener::bind(&deny_path).unwrap();
        let ok_server = tokio::spawn(async move {
            let (mut stream, _) = ok_listener.accept().await.unwrap();
            let mut buf = vec![0_u8; 256];
            let n = stream.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            assert!(req.starts_with("CONNECT 127.0.0.1:9"), "{req}");
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
            stream.write_all(b"pong").await.unwrap();
        });
        let deny_server = tokio::spawn(async move {
            let (mut stream, _) = deny_listener.accept().await.unwrap();
            let mut buf = vec![0_u8; 256];
            let _ = stream.read(&mut buf).await;
            stream
                .write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n")
                .await
                .unwrap();
        });

        // Sequential: SOCKET_PROXY_ENV is process-wide.
        std::env::set_var(SOCKET_PROXY_ENV, &ok_path);
        let mut sock = connect(
            SocketAddress {
                hostname: "127.0.0.1".into(),
                port: 9,
            },
            ConnectOptions::default(),
        )
        .await
        .expect("connect");
        let mut buf = [0_u8; 8];
        let n = sock.stream().read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"pong");
        sock.close().await.unwrap();
        ok_server.await.unwrap();

        std::env::set_var(SOCKET_PROXY_ENV, &deny_path);
        match connect(
            SocketAddress {
                hostname: "evil.example".into(),
                port: 22,
            },
            ConnectOptions::default(),
        )
        .await
        {
            Ok(_) => panic!("denied connect succeeded"),
            Err(err) => assert!(err.to_string().contains("403"), "{err}"),
        }
        deny_server.await.unwrap();
        std::env::remove_var(SOCKET_PROXY_ENV);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn connect_through_abstract_proxy() {
        use std::os::linux::net::SocketAddrExt;
        use std::os::unix::net::SocketAddr;

        let _guard = SOCKET_PROXY_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let name = format!(
            "bc-sdk-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let addr = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let listener = std::os::unix::net::UnixListener::bind_addr(&addr).unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::UnixListener::from_std(listener).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0_u8; 256];
            let n = stream.read(&mut buf).await.unwrap();
            assert!(
                String::from_utf8_lossy(&buf[..n]).starts_with("CONNECT 127.0.0.1:9"),
                "{}",
                String::from_utf8_lossy(&buf[..n])
            );
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
            stream.write_all(b"abs").await.unwrap();
        });
        std::env::set_var(
            SOCKET_PROXY_ENV,
            format!("{SOCKET_PROXY_ABSTRACT_PREFIX}{name}"),
        );
        let mut sock = connect(
            SocketAddress {
                hostname: "127.0.0.1".into(),
                port: 9,
            },
            ConnectOptions::default(),
        )
        .await
        .expect("abstract connect");
        let mut buf = [0_u8; 8];
        let n = sock.stream().read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"abs");
        sock.close().await.unwrap();
        server.await.unwrap();
        std::env::remove_var(SOCKET_PROXY_ENV);
    }
}
