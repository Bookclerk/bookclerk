//! Open a host-inherited sibling link (`fd:<n>` / `handle:<n>`) as a Tokio stream.

#![allow(unsafe_code)] // from_raw_fd / from_raw_handle on host-delivered descriptors.
#![allow(clippy::missing_docs_in_private_items)]

use anyhow::{bail, Context, Result};
use bookclerk_sandbox::LinkSpec;
use tokio::io::{AsyncRead, AsyncWrite};

/// Bidirectional end of a host-created RPC or proxy link.
#[derive(Debug)]
pub enum InheritedDuplex {
    /// Unix `socketpair` end.
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    /// Windows overlapped duplex pipe.
    #[cfg(windows)]
    Pipe(tokio::net::windows::named_pipe::NamedPipeClient),
}

impl tokio::io::AsyncRead for InheritedDuplex {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            #[cfg(unix)]
            Self::Unix(stream) => std::pin::Pin::new(stream).poll_read(cx, buf),
            #[cfg(windows)]
            Self::Pipe(stream) => std::pin::Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for InheritedDuplex {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            #[cfg(unix)]
            Self::Unix(stream) => std::pin::Pin::new(stream).poll_write(cx, buf),
            #[cfg(windows)]
            Self::Pipe(stream) => std::pin::Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            #[cfg(unix)]
            Self::Unix(stream) => std::pin::Pin::new(stream).poll_flush(cx),
            #[cfg(windows)]
            Self::Pipe(stream) => std::pin::Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            #[cfg(unix)]
            Self::Unix(stream) => std::pin::Pin::new(stream).poll_shutdown(cx),
            #[cfg(windows)]
            Self::Pipe(stream) => std::pin::Pin::new(stream).poll_shutdown(cx),
        }
    }
}

impl InheritedDuplex {
    /// Open `spec` (`fd:<n>` or `handle:<n>`) as a Tokio duplex.
    ///
    /// # Errors
    ///
    /// Returns an error when the spec is the wrong OS form or the descriptor
    /// cannot be wrapped.
    pub fn open(spec: &str) -> Result<Self> {
        match LinkSpec::parse(spec).with_context(|| format!("parse inherited link `{spec}`"))? {
            LinkSpec::Fd(fd) => open_fd(fd, spec),
            LinkSpec::Handle(handle) => open_handle(handle, spec),
        }
    }

    /// Open two unidirectional Windows handles (read half, then write half).
    ///
    /// Used for guest RPC and the socket-proxy mux. `read_spec` is this
    /// process's overlapped read half (`handle:<n>`). `write_spec` is the
    /// overlapped write half. They must be distinct handles: one duplex locks
    /// a pending `ReadFile` against `WriteFile`, so the next frame never
    /// completes.
    ///
    /// # Errors
    ///
    /// Returns an error when either spec is not a distinct `handle:<n>` or the
    /// handle cannot be wrapped as an overlapped named pipe.
    #[cfg(windows)]
    pub fn open_halves(
        read_spec: &str,
        write_spec: &str,
    ) -> Result<(
        Box<dyn AsyncRead + Unpin + Send>,
        Box<dyn AsyncWrite + Unpin + Send>,
    )> {
        let read_id = handle_id(read_spec)?;
        let write_id = handle_id(write_spec)?;
        if read_id == write_id {
            bail!("read and write handles must be distinct (both {read_id})");
        }
        let read = Self::open(read_spec).context("open inherited read half")?;
        let write = Self::open(write_spec).context("open inherited write half")?;
        let Self::Pipe(read_pipe) = read;
        let Self::Pipe(write_pipe) = write;
        Ok((Box::new(read_pipe), Box::new(write_pipe)))
    }

    /// Split into read and write halves for Cap'n Proto or the mux server.
    pub fn into_split(
        self,
    ) -> (
        Box<dyn AsyncRead + Unpin + Send>,
        Box<dyn AsyncWrite + Unpin + Send>,
    ) {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => {
                let (reader, writer) = stream.into_split();
                (Box::new(reader), Box::new(writer))
            }
            #[cfg(windows)]
            Self::Pipe(pipe) => {
                let (reader, writer) = tokio::io::split(pipe);
                (Box::new(reader), Box::new(writer))
            }
        }
    }
}

fn open_fd(fd: i32, spec: &str) -> Result<InheritedDuplex> {
    if cfg!(unix) {
        let _ = spec;
        #[cfg(unix)]
        {
            open_unix_fd(fd)
        }
        #[cfg(not(unix))]
        {
            unreachable!("cfg!(unix)")
        }
    } else {
        let _ = fd;
        bail!("fd: link specs are Unix-only (got `{spec}`)")
    }
}

#[cfg(windows)]
fn handle_id(spec: &str) -> Result<u64> {
    match LinkSpec::parse(spec).with_context(|| format!("parse inherited link `{spec}`"))? {
        LinkSpec::Handle(value) => Ok(value),
        LinkSpec::Fd(_) => bail!("Windows inherited half must be handle:<n>, got `{spec}`"),
    }
}

fn open_handle(handle: u64, spec: &str) -> Result<InheritedDuplex> {
    if cfg!(windows) {
        let _ = spec;
        #[cfg(windows)]
        {
            open_windows_handle(handle)
        }
        #[cfg(not(windows))]
        {
            unreachable!("cfg!(windows)")
        }
    } else {
        let _ = handle;
        bail!("handle: link specs are Windows-only (got `{spec}`)")
    }
}

#[cfg(unix)]
fn open_unix_fd(fd: i32) -> Result<InheritedDuplex> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let std = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
    std.set_nonblocking(true)
        .context("set inherited RPC/proxy fd non-blocking")?;
    // workerd is spawned without a handle list; CLOEXEC keeps SIGKILL of this
    // launcher from leaving the guest RPC/proxy ends open in the child.
    crate::unix_bind::set_cloexec(std.as_raw_fd())
        .context("set CLOEXEC on inherited RPC/proxy fd")?;
    let tokio = tokio::net::UnixStream::from_std(std).context("wrap inherited UnixStream")?;
    Ok(InheritedDuplex::Unix(tokio))
}

#[cfg(windows)]
fn open_windows_handle(value: u64) -> Result<InheritedDuplex> {
    use std::os::windows::io::RawHandle;
    let pipe = unsafe {
        tokio::net::windows::named_pipe::NamedPipeClient::from_raw_handle(
            value as usize as RawHandle,
        )?
    };
    Ok(InheritedDuplex::Pipe(pipe))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use bookclerk_sandbox::DuplexLink;

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_fd_spec_round_trips_bytes() {
        use std::os::fd::{FromRawFd, IntoRawFd};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (host, child) = DuplexLink::pair().expect("pair");
        let child_fd = child.into_owned_fd().into_raw_fd();
        let spec = format!("fd:{child_fd}");
        let opened = InheritedDuplex::open(&spec).expect("open child end");
        let flags = unsafe { libc::fcntl(child_fd, libc::F_GETFD) };
        assert!(
            flags >= 0 && flags & libc::FD_CLOEXEC != 0,
            "inherited link must be CLOEXEC so workerd cannot hold it"
        );
        let (mut reader, mut writer) = opened.into_split();

        let host_fd = host.into_owned_fd().into_raw_fd();
        let host_std = unsafe { std::os::unix::net::UnixStream::from_raw_fd(host_fd) };
        host_std.set_nonblocking(true).expect("nonblocking");
        let mut host_tok = tokio::net::UnixStream::from_std(host_std).expect("wrap host");

        writer.write_all(b"ping").await.expect("write");
        writer.flush().await.expect("flush");
        let mut buf = [0_u8; 4];
        host_tok.read_exact(&mut buf).await.expect("host read");
        assert_eq!(&buf, b"ping");
        host_tok.write_all(b"pong").await.expect("host write");
        reader.read_exact(&mut buf).await.expect("child read");
        assert_eq!(&buf, b"pong");
    }

    #[test]
    fn rejects_cross_os_spec() {
        #[cfg(unix)]
        {
            let err = InheritedDuplex::open("handle:1").expect_err("unix rejects handle");
            assert!(err.to_string().contains("Windows-only"), "{err}");
        }
        #[cfg(windows)]
        {
            let err = InheritedDuplex::open("fd:3").expect_err("windows rejects fd");
            assert!(err.to_string().contains("Unix-only"), "{err}");
        }
    }

    #[test]
    fn rejects_garbage_spec() {
        let err = InheritedDuplex::open("named:foo").expect_err("garbage");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("fd:") || msg.contains("handle:") || msg.contains("parse inherited link"),
            "{msg}"
        );
    }
}
