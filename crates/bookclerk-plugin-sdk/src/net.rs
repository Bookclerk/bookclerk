//! Mediated TCP sockets for native guests (Workers `connect()` equivalent).
//!
//! Native plugins must not call `socket(AF_INET)` / `connect` themselves. The
//! nested jail denies ambient internet; this module speaks HTTP CONNECT to the
//! host socket proxy (`BOOKCLERK_SOCKET_PROXY`) which applies the same
//! [`bookclerk_plugin_manifest::EgressPolicy`] as workerd `fetch()`/`connect()`.

#![allow(clippy::missing_docs_in_private_items)]

use crate::error::{Result, SdkError};

/// Env var set by `bookclerk-workerd` for native-behind-workerd guests.
pub const SOCKET_PROXY_ENV: &str = "BOOKCLERK_SOCKET_PROXY";

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
    #[cfg(unix)]
    stream: tokio::net::UnixStream,
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
        #[cfg(unix)]
        {
            use tokio::io::AsyncWriteExt;
            let mut stream = self.stream;
            stream.shutdown().await?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = self;
            Err(SdkError::message("native sockets require Unix"))
        }
    }

    /// Borrow the proxied stream (past CONNECT). Unix only.
    #[cfg(unix)]
    #[must_use]
    pub fn stream(&mut self) -> &mut tokio::net::UnixStream {
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
    #[cfg(not(unix))]
    {
        let _ = (address, options);
        return Err(SdkError::message(
            "native TCP sockets are Unix-only in this Bookclerk build",
        ));
    }
    #[cfg(unix)]
    {
        use tokio::io::AsyncWriteExt;
        use tokio::net::UnixStream;

        let path = std::env::var(SOCKET_PROXY_ENV).map_err(|_| {
            SdkError::message(
                "BOOKCLERK_SOCKET_PROXY is unset; native TCP requires native-behind-workerd",
            )
        })?;
        let mut stream = UnixStream::connect(&path).await?;
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
                "SecureTransport::On: CONNECT succeeded; wrap the stream with tokio-rustls \
(workerd authors should use starttls + socket.startTls())",
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
/// Reads the CONNECT response header from the socket proxy.
///
/// # Errors
///
/// Returns an error when the stream ends early, the handshake exceeds 8 KiB,
/// or the proxy returns a non-success status.
async fn read_http_head(stream: &mut tokio::net::UnixStream) -> Result<String> {
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
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn connect_through_fake_proxy_and_403() {
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
}
