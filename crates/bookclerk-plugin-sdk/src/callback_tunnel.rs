//! Multiplexed byte tunnel for host-owned OAuth callback listeners.
//!
//! Thin wrappers around [`crate::mux`]: the host [`TunnelHost::open`]s one
//! stream per accepted browser connection; the guest [`TunnelGuest::accept`]s
//! them. Flow control is the mux per-stream credit window.

use tokio::io::{AsyncRead, AsyncWrite};

use crate::mux::{Mux, MuxStream};
use crate::Result;

/// Host end of the callback tunnel: opens logical connections toward the guest.
pub struct TunnelHost {
    /// Client mux (odd connection ids) shared with [`TunnelGuest`].
    mux: Mux,
}

/// Guest end of the callback tunnel: accepts logical connections from the host.
pub struct TunnelGuest {
    /// Server mux (even connection ids) shared with [`TunnelHost`].
    mux: Mux,
}

/// One multiplexed logical connection implementing `AsyncRead` + `AsyncWrite`.
pub type TunnelStream = MuxStream;

impl TunnelHost {
    /// Spawns reader/writer tasks on the host half of a duplex IPC link.
    ///
    /// # Arguments
    ///
    /// * `reader` - Async half that receives guest→host frames.
    /// * `writer` - Async half that sends host→guest frames.
    pub fn new<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        Self {
            mux: Mux::client(reader, writer),
        }
    }

    /// Opens one logical connection toward the guest and returns its stream.
    ///
    /// # Errors
    ///
    /// Returns [`crate::SdkError`] when the mux writer has exited.
    pub async fn open(&self) -> Result<TunnelStream> {
        self.mux.open().await
    }
}

impl TunnelGuest {
    /// Spawns reader/writer tasks on the guest half of a duplex IPC link.
    ///
    /// # Arguments
    ///
    /// * `reader` - Async half that receives host→guest frames.
    /// * `writer` - Async half that sends guest→host frames.
    pub fn new<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        Self {
            mux: Mux::server(reader, writer),
        }
    }

    /// Waits for the next logical connection opened by [`TunnelHost::open`].
    ///
    /// # Errors
    ///
    /// Returns [`crate::SdkError`] when the mux reader exits.
    pub async fn accept(&mut self) -> Result<TunnelStream> {
        self.mux.accept().await
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn host_open_guest_accept_echo() {
        let (a, b) = duplex(64 * 1024);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let host = TunnelHost::new(ar, aw);
        let mut guest = TunnelGuest::new(br, bw);

        let mut host_stream = host.open().await.expect("open");
        let mut guest_stream = guest.accept().await.expect("accept");

        host_stream.write_all(b"hello").await.expect("write");
        let mut buf = [0u8; 5];
        guest_stream.read_exact(&mut buf).await.expect("read");
        assert_eq!(&buf, b"hello");

        guest_stream.write_all(b"world").await.expect("write");
        let mut buf2 = [0u8; 5];
        host_stream.read_exact(&mut buf2).await.expect("read");
        assert_eq!(&buf2, b"world");
    }
}
