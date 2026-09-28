//! Mediated TCP sockets for native guests (Workers `connect()` equivalent).
//!
//! Native plugins must not call `socket(AF_INET)` / `connect` themselves. The
//! guest jail denies ambient internet; this module speaks HTTP CONNECT through
//! `BOOKCLERK_SOCKET_PROXY`, which applies the same
//! [`bookclerk_plugin_manifest::EgressPolicy`] as workerd `fetch()`/`connect()`.
//!
//! Production native-behind-workerd sets `fd:<n>` or `handle:<n>` and multiplexes
//! CONNECT streams over that inherited link ([`crate::mux`]). A per-session
//! challenge ([`SESSION_CHALLENGE_ENV`]) is written before the first mux frame
//! so a numeric descriptor is not an identity. Pathname, `abstract:`, and
//! `\\.\pipe\` forms remain for tests and older launchers. Native plugins must
//! be rebuilt against this SDK; there is no ambient TCP fallback.

#![allow(clippy::missing_docs_in_private_items)]
#![allow(unsafe_code)] // inherited fd:/handle: → UnixStream / NamedPipeClient.

use crate::error::{Result, SdkError};

#[cfg(all(test, any(unix, feature = "http")))]
use std::sync::Mutex;

/// Env var set by `bookclerk-workerd` for native-behind-workerd guests.
pub const SOCKET_PROXY_ENV: &str = "BOOKCLERK_SOCKET_PROXY";
/// Windows write half of the inherited socket-proxy mux (`handle:<n>`).
///
/// Unset on Unix and in tests that pass one duplex `handle:`. Product spawns
/// set it so the guest does not read and write the same named pipe.
pub const SOCKET_PROXY_WRITE_ENV: &str = "BOOKCLERK_SOCKET_PROXY_WRITE";

/// Optional fail-closed flag: deny ambient TCP when [`SOCKET_PROXY_ENV`] is unset.
///
/// The host no longer sets this (sibling Deny + inherited `SOCKET_PROXY` is
/// the production contract). SDK HTTP and native TCP still fail closed when
/// this is `1` without a proxy, so tests and older launchers cannot fall
/// through to `socket(AF_INET)`.
pub const NESTED_NATIVE_JAIL_ENV: &str = "BOOKCLERK_NESTED_NATIVE_JAIL";

/// Host-created directory for guest pathname sockets.
///
/// The native-behind-workerd guest may bind and connect Unix sockets only in
/// this directory (OAuth callback and the macOS Postgres mediator). Linux
/// Postgres still splices through `/proc/self/fd`. Unset on Windows.
pub const GUEST_IPC_DIR_ENV: &str = "BOOKCLERK_GUEST_IPC_DIR";

/// Hex-encoded 32-byte secret for the inherited proxy mux.
///
/// The guest writes these bytes before any mux frame. The host accepts the
/// link only when they match the secret it put in this process's environment.
/// An unrelated child, or another session's guest, does not have this value.
/// A numeric `fd:` / `handle:` is not an identity across processes.
pub const SESSION_CHALLENGE_ENV: &str = "BOOKCLERK_SESSION_CHALLENGE";

/// Length of [`SESSION_CHALLENGE_ENV`] before hex encoding.
pub const SESSION_CHALLENGE_LEN: usize = 32;

/// Test-only extra endpoint inherited beside [`SOCKET_PROXY_ENV`].
///
/// Unix value is `fd:<n>`. Windows value is `handle:<n>`, with the write half
/// in [`TEST_EXTRA_ENDPOINT_WRITE_ENV`]. The launcher sets these only when it
/// actually transferred the endpoint. Unset is not an observation: the guest
/// classifies the slot named by [`TEST_EXTRA_CANDIDATE_ENV`] instead, and
/// reports `not-run` when that metadata is missing. The guest does not write
/// [`SESSION_CHALLENGE_ENV`] to the extra slot: the fixture authenticates the
/// endpoint with its own challenge before the guest opens the mux.
pub const TEST_EXTRA_ENDPOINT_ENV: &str = "BOOKCLERK_TEST_EXTRA_ENDPOINT";

/// Windows write half of [`TEST_EXTRA_ENDPOINT_ENV`] (`handle:<n>`).
pub const TEST_EXTRA_ENDPOINT_WRITE_ENV: &str = "BOOKCLERK_TEST_EXTRA_ENDPOINT_WRITE";

/// Test-only identity of the candidate endpoint slot to classify.
///
/// Unix value is `fd:<n>`. Windows value is `handle:<n>`, with the write half
/// in [`TEST_EXTRA_CANDIDATE_WRITE_ENV`]. This names the slot. It does not
/// grant the launcher permission to transfer the endpoint. Missing metadata
/// is `not-run`, not proof the endpoint is absent. A live object at the slot
/// that is not the candidate is an identification failure, not absence.
pub const TEST_EXTRA_CANDIDATE_ENV: &str = "BOOKCLERK_TEST_EXTRA_CANDIDATE";

/// Windows write half of [`TEST_EXTRA_CANDIDATE_ENV`] (`handle:<n>`).
pub const TEST_EXTRA_CANDIDATE_WRITE_ENV: &str = "BOOKCLERK_TEST_EXTRA_CANDIDATE_WRITE";

/// True when [`NESTED_NATIVE_JAIL_ENV`] is `1`.
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
#[cfg(any(unix, windows))]
pub enum ProxyStream {
    /// Pathname / abstract Unix socket (tests and older launchers).
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    /// Windows named-pipe pathname (tests and older launchers).
    #[cfg(windows)]
    Pipe(tokio::net::windows::named_pipe::NamedPipeClient),
    /// Inherited `fd:` / `handle:` multiplexed link.
    Mux(crate::mux::MuxStream),
}

#[cfg(any(unix, windows))]
impl tokio::io::AsyncRead for ProxyStream {
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
            Self::Mux(stream) => std::pin::Pin::new(stream).poll_read(cx, buf),
        }
    }
}

#[cfg(any(unix, windows))]
impl tokio::io::AsyncWrite for ProxyStream {
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
            Self::Mux(stream) => std::pin::Pin::new(stream).poll_write(cx, buf),
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
            Self::Mux(stream) => std::pin::Pin::new(stream).poll_flush(cx),
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
            Self::Mux(stream) => std::pin::Pin::new(stream).poll_shutdown(cx),
        }
    }
}

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

    /// Split into owned reader/writer halves (pathname Unix sockets only).
    ///
    /// Production native plugins use the mux `fd:` / `handle:` link and
    /// [`Self::into_stream`]. This does not open a TCP socket.
    ///
    /// # Errors
    ///
    /// Returns an unsupported-transport error when the proxy is a multiplexed
    /// `fd:` / `handle:` link.
    #[cfg(unix)]
    pub fn into_split(
        self,
    ) -> Result<(
        tokio::net::unix::OwnedReadHalf,
        tokio::net::unix::OwnedWriteHalf,
    )> {
        match self.stream {
            ProxyStream::Unix(stream) => Ok(stream.into_split()),
            ProxyStream::Mux(_) => Err(unsupported_pathname_transport("into_split")),
        }
    }

    /// Consumes the socket, returning the CONNECT-established proxy stream.
    #[cfg(any(unix, windows))]
    #[must_use]
    pub fn into_stream(self) -> ProxyStream {
        self.stream
    }

    /// Consumes the socket, returning the CONNECT-established Unix stream.
    ///
    /// Production native plugins use [`Self::into_stream`]. This does not open
    /// a TCP socket when the proxy is a mux link.
    ///
    /// # Errors
    ///
    /// Returns an unsupported-transport error when the proxy is a multiplexed
    /// `fd:` / `handle:` link.
    #[cfg(unix)]
    pub fn into_unix_stream(self) -> Result<tokio::net::UnixStream> {
        match self.into_stream() {
            ProxyStream::Unix(stream) => Ok(stream),
            ProxyStream::Mux(_) => Err(unsupported_pathname_transport("into_unix_stream")),
        }
    }

    /// Start TLS on a `starttls` socket.
    ///
    /// The proxy is byte-transparent, so TLS is end-to-end. Wrap
    /// [`Self::into_stream`] with `tokio-rustls` (same pattern as workerd
    /// `socket.startTls()` after `secureTransport: "starttls"`). Pathname
    /// sockets may use [`Self::into_split`]. Mux links have no TCP fallback.
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
            "native startTls: wrap PluginSocket::into_stream() with tokio-rustls (proxy is byte-transparent; into_split is pathname-only)",
        ))
    }
}

/// `into_split` / `into_unix_stream` are pathname transports. Mux stays on [`PluginSocket::into_stream`].
#[cfg(unix)]
fn unsupported_pathname_transport(method: &str) -> SdkError {
    SdkError::message(format!(
        "unsupported transport: PluginSocket::{method} requires a Unix pathname SOCKET_PROXY; production native plugins use mux fd:/handle: and PluginSocket::into_stream"
    ))
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

#[cfg(any(unix, windows))]
/// Connects to the host socket proxy (`fd:`/`handle:`, pathname, or `abstract:`).
///
/// # Errors
///
/// Returns an I/O error when the proxy cannot be reached.
async fn connect_proxy(spec: &str) -> Result<ProxyStream> {
    if spec.starts_with("fd:") || spec.starts_with("handle:") {
        return open_mux_proxy(spec).await;
    }
    #[cfg(unix)]
    {
        use tokio::net::UnixStream;
        if let Some(name) = spec.strip_prefix(SOCKET_PROXY_ABSTRACT_PREFIX) {
            return Ok(ProxyStream::Unix(connect_abstract(name).await?));
        }
        return Ok(ProxyStream::Unix(UnixStream::connect(spec).await?));
    }
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        let name = spec.strip_prefix("pipe:").unwrap_or(spec);
        Ok(ProxyStream::Pipe(ClientOptions::new().open(name)?))
    }
}

/// Process-wide mux over the inherited `fd:` / `handle:` proxy link.
///
/// # Errors
///
/// Returns [`SdkError`] when the inherited descriptor cannot be opened, or when
/// an earlier open of this process's proxy link failed.
#[cfg(any(unix, windows))]
async fn shared_mux(spec: &str) -> Result<crate::mux::Mux> {
    use tokio::sync::OnceCell;

    static MUX: OnceCell<std::result::Result<crate::mux::Mux, String>> = OnceCell::const_new();
    let stored = MUX
        .get_or_init(|| async {
            match open_inherited_mux(spec).await {
                Ok(mux) => Ok(mux),
                Err(err) => Err(err.to_string()),
            }
        })
        .await;
    match stored {
        Ok(mux) => Ok(mux.clone()),
        Err(err) => Err(SdkError::message(err.clone())),
    }
}

/// Opens one logical stream on the process-wide inherited mux.
///
/// # Errors
///
/// Returns [`SdkError`] when the mux is unavailable or the writer task has exited.
#[cfg(any(unix, windows))]
async fn open_mux_proxy(spec: &str) -> Result<ProxyStream> {
    let mux = shared_mux(spec).await?;
    Ok(ProxyStream::Mux(mux.open().await?))
}

/// Builds a client mux from an `fd:` or `handle:` link spec.
///
/// When [`SESSION_CHALLENGE_ENV`] is set, those bytes are written before the
/// mux takes the writer.
///
/// # Errors
///
/// Returns [`SdkError`] when the spec is the wrong OS form, the number does not
/// parse, the challenge is malformed, or the descriptor cannot be wrapped.
#[cfg(any(unix, windows))]
async fn open_inherited_mux(spec: &str) -> Result<crate::mux::Mux> {
    if let Some(rest) = spec.strip_prefix("fd:") {
        #[cfg(unix)]
        {
            let fd: i32 = rest
                .parse()
                .map_err(|_| SdkError::message(format!("invalid fd link spec {spec}")))?;
            return mux_from_unix_fd(fd).await;
        }
        #[cfg(not(unix))]
        {
            let _ = rest;
            return Err(SdkError::message("fd: SOCKET_PROXY is Unix-only"));
        }
    }
    if let Some(rest) = spec.strip_prefix("handle:") {
        #[cfg(windows)]
        {
            if let Ok(write_spec) = std::env::var(SOCKET_PROXY_WRITE_ENV) {
                if !write_spec.is_empty() {
                    return mux_from_windows_halves(spec, &write_spec).await;
                }
            }
            let value: u64 = rest
                .parse()
                .map_err(|_| SdkError::message(format!("invalid handle link spec {spec}")))?;
            return mux_from_windows_handle(value).await;
        }
        #[cfg(not(windows))]
        {
            let _ = rest;
            return Err(SdkError::message("handle: SOCKET_PROXY is Windows-only"));
        }
    }
    Err(SdkError::message(format!(
        "socket proxy spec must be fd:<n> or handle:<n>, got {spec}"
    )))
}

/// Takes ownership of `fd`, marks it `CLOEXEC`, and multiplexes it.
///
/// # Errors
///
/// Returns [`SdkError`] when the descriptor cannot be made non-blocking or
/// wrapped as a Tokio stream, or when the session challenge cannot be written.
#[cfg(unix)]
async fn mux_from_unix_fd(fd: i32) -> Result<crate::mux::Mux> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let std = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
    set_inherited_cloexec(std.as_raw_fd())?;
    std.set_nonblocking(true)?;
    let tokio = tokio::net::UnixStream::from_std(std)?;
    let (reader, writer) = tokio.into_split();
    finish_client_mux(reader, writer).await
}

/// `FD_CLOEXEC` on a descriptor this process just adopted.
///
/// The host cleared it so `exec` could deliver the link. Grandchildren must
/// not inherit it.
///
/// # Errors
///
/// Returns [`SdkError`] when `F_GETFD` or `F_SETFD` fails.
#[cfg(unix)]
pub(crate) fn set_inherited_cloexec(fd: i32) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(SdkError::message(format!(
            "F_GETFD on inherited descriptor: {}",
            std::io::Error::last_os_error()
        )));
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(SdkError::message(format!(
            "FD_CLOEXEC on inherited descriptor: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// Writes [`SESSION_CHALLENGE_ENV`] when set, then starts the client mux.
///
/// # Errors
///
/// Returns [`SdkError`] when the challenge is set but is not 32 bytes of hex,
/// or when writing those bytes fails.
#[cfg(any(unix, windows))]
async fn finish_client_mux<R, W>(reader: R, mut writer: W) -> Result<crate::mux::Mux>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use tokio::io::AsyncWriteExt;
    if let Some(bytes) = session_challenge_bytes()? {
        writer.write_all(&bytes).await?;
    }
    Ok(crate::mux::Mux::client(reader, writer))
}

/// Decodes [`SESSION_CHALLENGE_ENV`]. `Ok(None)` when it is unset.
///
/// # Errors
///
/// Returns [`SdkError`] when the variable is set but is not 32 bytes of hex.
#[cfg(any(unix, windows))]
fn session_challenge_bytes() -> Result<Option<[u8; SESSION_CHALLENGE_LEN]>> {
    let Ok(text) = std::env::var(SESSION_CHALLENGE_ENV) else {
        return Ok(None);
    };
    let text = text.trim();
    if text.len() != SESSION_CHALLENGE_LEN * 2 || !text.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(SdkError::message(format!(
            "{SESSION_CHALLENGE_ENV} must be {SESSION_CHALLENGE_LEN} bytes of hex"
        )));
    }
    let mut out = [0u8; SESSION_CHALLENGE_LEN];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).map_err(|_| {
            SdkError::message(format!(
                "{SESSION_CHALLENGE_ENV} must be {SESSION_CHALLENGE_LEN} bytes of hex"
            ))
        })?;
    }
    Ok(Some(out))
}

/// Takes ownership of `value` and multiplexes it as a client link.
///
/// # Errors
///
/// Returns [`SdkError`] when the handle cannot be wrapped as a Tokio pipe.
#[cfg(windows)]
async fn mux_from_windows_handle(value: u64) -> Result<crate::mux::Mux> {
    let pipe = windows_pipe_client(value)?;
    let (reader, writer) = tokio::io::split(pipe);
    finish_client_mux(reader, writer).await
}

/// Multiplexes two unidirectional Windows handles (read, then write).
///
/// # Errors
///
/// Returns [`SdkError`] when either spec is not a distinct `handle:<n>` or a
/// handle cannot be wrapped.
#[cfg(windows)]
async fn mux_from_windows_halves(read_spec: &str, write_spec: &str) -> Result<crate::mux::Mux> {
    let read_id = windows_handle_id(read_spec)?;
    let write_id = windows_handle_id(write_spec)?;
    if read_id == write_id {
        return Err(SdkError::message(
            "socket proxy read and write handles must be distinct",
        ));
    }
    let read = windows_pipe_client(read_id)?;
    let write = windows_pipe_client(write_id)?;
    finish_client_mux(read, write).await
}

#[cfg(windows)]
fn windows_handle_id(spec: &str) -> Result<u64> {
    let rest = spec.strip_prefix("handle:").ok_or_else(|| {
        SdkError::message(format!("socket proxy half must be handle:<n>, got {spec}"))
    })?;
    rest.parse()
        .map_err(|_| SdkError::message(format!("invalid handle link spec {spec}")))
}

#[cfg(windows)]
fn windows_pipe_client(value: u64) -> Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    use std::os::windows::io::RawHandle;
    // Same rule as Unix `FD_CLOEXEC`: adopting the inherited proxy must stop
    // a later `CreateProcess` in this guest from receiving it. The jail marks
    // the handle inheritable only so this process can receive it.
    clear_inherited_handle(value)?;
    unsafe {
        tokio::net::windows::named_pipe::NamedPipeClient::from_raw_handle(
            value as usize as RawHandle,
        )
    }
    .map_err(SdkError::from)
}

/// Clear `HANDLE_FLAG_INHERIT` on a handle this process just adopted.
///
/// # Errors
///
/// Returns [`SdkError`] when `SetHandleInformation` fails.
#[cfg(windows)]
fn clear_inherited_handle(value: u64) -> Result<()> {
    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
    extern "system" {
        fn SetHandleInformation(handle: *mut core::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    let handle = value as usize as *mut core::ffi::c_void;
    let ok = unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
    if ok == 0 {
        return Err(SdkError::message(format!(
            "SetHandleInformation(clear inherit) on handle {value}: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
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

    #[tokio::test]
    async fn connect_through_inherited_fd_mux() {
        use std::os::fd::IntoRawFd;

        let _guard = SOCKET_PROXY_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let (guest, gateway) = tokio::net::UnixStream::pair().expect("pair");
        let guest_fd = guest.into_std().expect("into_std").into_raw_fd();
        let (gr, gw) = gateway.into_split();
        let server = crate::mux::Mux::server(gr, gw);
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let server_task = tokio::spawn(async move {
            let mut stream = server.accept().await.expect("accept");
            let mut buf = vec![0_u8; 256];
            let n = stream.read(&mut buf).await.expect("read connect");
            assert!(
                String::from_utf8_lossy(&buf[..n]).starts_with("CONNECT 127.0.0.1:9"),
                "{}",
                String::from_utf8_lossy(&buf[..n])
            );
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .expect("200");
            stream.write_all(b"mux").await.expect("body");
            // Mux drop cancels the writer and does not flush. Shutdown writes
            // the queued response while `server` is still alive, and the mux
            // stays up until the client has closed so that Close can be written.
            stream.shutdown().await.expect("server shutdown");
            drop(stream);
            let _ = release_rx.await;
            drop(server);
        });

        std::env::remove_var(SESSION_CHALLENGE_ENV);
        std::env::set_var(SOCKET_PROXY_ENV, format!("fd:{guest_fd}"));
        let mut sock = connect(
            SocketAddress {
                hostname: "127.0.0.1".into(),
                port: 9,
            },
            ConnectOptions::default(),
        )
        .await
        .expect("mux connect");
        let mut buf = [0_u8; 8];
        let n = sock.stream().read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"mux");
        sock.close().await.unwrap();
        let _ = release_tx.send(());
        server_task.await.unwrap();
        std::env::remove_var(SOCKET_PROXY_ENV);
    }

    #[tokio::test]
    async fn pathname_split_works_and_mux_has_no_tcp_fallback() {
        let (unix_peer, other) = tokio::net::UnixStream::pair().expect("pair");
        let pathname = PluginSocket {
            stream: ProxyStream::Unix(unix_peer),
            secure: SecureTransport::Off,
            started_tls: false,
        };
        pathname.into_split().expect("pathname into_split");
        let again = PluginSocket {
            stream: ProxyStream::Unix(other),
            secure: SecureTransport::Off,
            started_tls: false,
        };
        again.into_unix_stream().expect("pathname into_unix_stream");

        let (left, right) = tokio::net::UnixStream::pair().expect("pair");
        let (server_read, server_write) = left.into_split();
        let _server = crate::mux::Mux::server(server_read, server_write);
        let (client_read, client_write) = right.into_split();
        let client = crate::mux::Mux::client(client_read, client_write);
        let opened = client.open().await.expect("mux open");
        let mux_sock = PluginSocket {
            stream: ProxyStream::Mux(opened),
            secure: SecureTransport::Off,
            started_tls: false,
        };
        let err = mux_sock.into_split().expect_err("mux into_split");
        assert_mux_transport(&err.to_string());

        let opened = client.open().await.expect("second mux open");
        let mux_sock = PluginSocket {
            stream: ProxyStream::Mux(opened),
            secure: SecureTransport::Off,
            started_tls: false,
        };
        let err = mux_sock
            .into_unix_stream()
            .expect_err("mux into_unix_stream");
        assert_mux_transport(&err.to_string());

        let opened = client.open().await.expect("third mux open");
        let mux_sock = PluginSocket {
            stream: ProxyStream::Mux(opened),
            secure: SecureTransport::Off,
            started_tls: false,
        };
        assert!(matches!(mux_sock.into_stream(), ProxyStream::Mux(_)));
    }

    fn assert_mux_transport(msg: &str) {
        assert!(msg.contains("unsupported transport"), "{msg}");
        assert!(msg.contains("fd:") && msg.contains("handle:"), "{msg}");
        assert!(!msg.contains("127.0.0.1"), "{msg}");
    }

    /// Adopting an inherited `fd:` writes the session challenge and sets `FD_CLOEXEC`.
    #[tokio::test]
    async fn inherited_fd_writes_the_session_challenge_and_is_cloexec() {
        use std::os::fd::IntoRawFd;
        use tokio::io::AsyncReadExt;

        struct ClearChallenge;
        impl Drop for ClearChallenge {
            fn drop(&mut self) {
                std::env::remove_var(SESSION_CHALLENGE_ENV);
            }
        }

        let _guard = SOCKET_PROXY_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _clear = ClearChallenge;

        let (guest, mut host) = tokio::net::UnixStream::pair().expect("pair");
        let guest_fd = guest.into_std().expect("into_std").into_raw_fd();
        let challenge = [0xab_u8; SESSION_CHALLENGE_LEN];
        let hex_challenge: String = challenge.iter().map(|byte| format!("{byte:02x}")).collect();
        std::env::set_var(SESSION_CHALLENGE_ENV, &hex_challenge);

        let mux = mux_from_unix_fd(guest_fd).await.expect("client mux");
        let flags = unsafe { libc::fcntl(guest_fd, libc::F_GETFD) };
        assert!(flags >= 0, "F_GETFD on the adopted descriptor");
        assert_ne!(
            flags & libc::FD_CLOEXEC,
            0,
            "an adopted fd must not stay inheritable"
        );
        let mut got = [0u8; SESSION_CHALLENGE_LEN];
        host.read_exact(&mut got).await.expect("challenge preamble");
        assert_eq!(got, challenge);
        drop(mux);
    }
}
