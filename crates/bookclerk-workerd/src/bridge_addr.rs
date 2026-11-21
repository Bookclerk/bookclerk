//! Where the launcher reaches workerd's bridge RPC socket, and the stream it
//! gets.
//!
//! On Unix the bridge is a Unix socket the launcher binds and hands to
//! workerd with `--socket-fd`, so neither process needs an IP socket and the
//! jail can deny them all. Windows workerd has no Unix sockets, so there the
//! bridge stays on in-container loopback TCP.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

/// Address of the bridge RPC socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeAddr {
    /// `127.0.0.1:port`: Windows, and unconfined smoke runs.
    Tcp(u16),
    /// A Linux abstract-namespace Unix socket name (no leading NUL).
    #[cfg(target_os = "linux")]
    Abstract(String),
    /// A pathname Unix socket.
    #[cfg(unix)]
    Path(std::path::PathBuf),
}

impl BridgeAddr {
    /// `Host` header value for requests on this socket.
    #[must_use]
    pub fn host_header(&self) -> String {
        match self {
            Self::Tcp(port) => format!("127.0.0.1:{port}"),
            #[cfg(target_os = "linux")]
            Self::Abstract(_) => "localhost".to_string(),
            #[cfg(unix)]
            Self::Path(_) => "localhost".to_string(),
        }
    }

    /// Opens a fresh connection to the bridge.
    ///
    /// # Errors
    ///
    /// When the connection is refused or the address can't be formed.
    pub async fn connect(&self) -> io::Result<BridgeStream> {
        match self {
            Self::Tcp(port) => Ok(BridgeStream::Tcp(
                TcpStream::connect(("127.0.0.1", *port)).await?,
            )),
            #[cfg(target_os = "linux")]
            Self::Abstract(name) => {
                use std::os::linux::net::SocketAddrExt;
                let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())?;
                // A local Unix connect completes at once; it never blocks on
                // the network.
                let std = std::os::unix::net::UnixStream::connect_addr(&addr)?;
                std.set_nonblocking(true)?;
                Ok(BridgeStream::Unix(tokio::net::UnixStream::from_std(std)?))
            }
            #[cfg(unix)]
            Self::Path(path) => Ok(BridgeStream::Unix(
                crate::unix_bind::connect_pathname(path).await?,
            )),
        }
    }
}

impl std::fmt::Display for BridgeAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp(port) => write!(f, "127.0.0.1:{port}"),
            #[cfg(target_os = "linux")]
            Self::Abstract(name) => write!(f, "unix-abstract:{name}"),
            #[cfg(unix)]
            Self::Path(path) => write!(f, "unix:{}", path.display()),
        }
    }
}

/// A connection to the bridge.
#[derive(Debug)]
pub enum BridgeStream {
    /// Loopback TCP.
    Tcp(TcpStream),
    /// Unix socket.
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
}

impl AsyncRead for BridgeStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(unix)]
            Self::Unix(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for BridgeStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Tcp(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(unix)]
            Self::Unix(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(s) => Pin::new(s).poll_flush(cx),
            #[cfg(unix)]
            Self::Unix(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(unix)]
            Self::Unix(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
#[allow(clippy::missing_panics_doc, clippy::expect_used)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Writes `ping` on `stream`, reads the peer's `pong`, then shuts the
    /// write half down, driving every `AsyncRead` / `AsyncWrite` arm.
    async fn exchange(mut stream: BridgeStream) {
        stream.write_all(b"ping").await.expect("write");
        stream.flush().await.expect("flush");
        let mut reply = [0u8; 4];
        stream.read_exact(&mut reply).await.expect("read");
        assert_eq!(&reply, b"pong");
        stream.shutdown().await.expect("shutdown");
    }

    /// Answers one `ping` with `pong` on a std Unix stream.
    fn answer(mut server: std::os::unix::net::UnixStream) {
        use std::io::{Read, Write};
        let mut got = [0u8; 4];
        server.read_exact(&mut got).expect("server read");
        assert_eq!(&got, b"ping");
        server.write_all(b"pong").expect("server write");
    }

    #[test]
    fn every_address_names_its_host_and_form() {
        let tcp = BridgeAddr::Tcp(8123);
        assert_eq!(tcp.host_header(), "127.0.0.1:8123");
        assert_eq!(tcp.to_string(), "127.0.0.1:8123");
        let abstract_name = BridgeAddr::Abstract("bc-r-1".into());
        assert_eq!(abstract_name.host_header(), "localhost");
        assert_eq!(abstract_name.to_string(), "unix-abstract:bc-r-1");
        let path = BridgeAddr::Path("/run/x/rpc.sock".into());
        assert_eq!(path.host_header(), "localhost");
        assert_eq!(path.to_string(), "unix:/run/x/rpc.sock");
    }

    #[tokio::test]
    async fn tcp_bridge_carries_bytes_both_ways() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut got = [0u8; 4];
            socket.read_exact(&mut got).await.expect("server read");
            assert_eq!(&got, b"ping");
            socket.write_all(b"pong").await.expect("server write");
        });
        exchange(BridgeAddr::Tcp(port).connect().await.expect("connect")).await;
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn abstract_and_path_bridges_carry_bytes_both_ways() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let (addr, listener) = crate::unix_bind::bind_rpc(dir.path()).expect("bind rpc");
        let stream = addr.connect().await.expect("connect abstract");
        let (server, _) = listener.accept().expect("accept abstract");
        let task = tokio::task::spawn_blocking(move || answer(server));
        exchange(stream).await;
        task.await.expect("server task");

        let sock = dir.path().join("rpc.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind path");
        let stream = BridgeAddr::Path(sock)
            .connect()
            .await
            .expect("connect path");
        let (server, _) = listener.accept().expect("accept path");
        let task = tokio::task::spawn_blocking(move || answer(server));
        exchange(stream).await;
        task.await.expect("server task");
    }
}
