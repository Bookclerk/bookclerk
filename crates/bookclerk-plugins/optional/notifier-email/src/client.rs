//! Moves an SMTP [`Session`]'s bytes over a connection.
//!
//! [`converse`] reads replies, feeds them to the session and writes what it
//! answers, upgrading the connection when it asks for TLS. The connection
//! comes from a [`Transport`]: [`HostTransport`] in the guest (a TCP tunnel
//! through the host, which checks it against the approved `server`
//! destination, then rustls), and in-memory pipes in the tests.

use std::time::Duration;

use async_trait::async_trait;
use bookclerk_plugin_sdk::PluginError;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::smtp::{read_reply, HostPort, Security, Session, Step};

/// How long the server may take to answer one step.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// Most unread bytes held from the server; a server that sends more without
/// waiting for us is refused.
pub const MAX_BUFFERED_BYTES: usize = 64 * 1024;

/// A byte stream both ways.
pub trait Io: AsyncRead + AsyncWrite + Unpin {}

impl<T: AsyncRead + AsyncWrite + Unpin> Io for T {}

/// A connection, plain or TLS.
pub type BoxIo = Box<dyn Io>;

/// Opens connections and upgrades them to TLS.
#[async_trait(?Send)]
pub trait Transport {
    /// Connects to `server`; for [`Security::Tls`] the connection is TLS
    /// from the first byte.
    ///
    /// # Errors
    ///
    /// When the server can't be reached or TLS fails.
    async fn connect(&self, server: &HostPort, security: Security) -> Result<BoxIo, PluginError>;

    /// Upgrades a plain connection to TLS, verifying the certificate for
    /// `hostname`.
    ///
    /// # Errors
    ///
    /// When the handshake fails.
    async fn start_tls(&self, hostname: &str, io: BoxIo) -> Result<BoxIo, PluginError>;
}

/// Waits for `future`, at most `limit`.
///
/// # Arguments
///
/// * `limit` - The longest wait.
/// * `step` - What it is, for the error.
/// * `future` - The step.
///
/// # Errors
///
/// `unavailable` when the server takes too long, or the step's own error.
async fn timed<T>(
    limit: Duration,
    step: &str,
    future: impl std::future::Future<Output = Result<T, PluginError>>,
) -> Result<T, PluginError> {
    tokio::time::timeout(limit, future).await.map_err(|_| {
        PluginError::unavailable(format!("the server did not answer {step} in time"))
    })?
}

/// Runs one session over a fresh connection to `server`.
///
/// # Arguments
///
/// * `transport` - Where connections come from.
/// * `server` - The approved server.
/// * `security` - How the connection is protected.
/// * `session` - The protocol state.
/// * `limit` - How long each step may take ([`STEP_TIMEOUT`] in the guest).
///
/// # Errors
///
/// When the server refuses, misbehaves, or can't be reached; the session's
/// mapping decides which errors the host retries.
pub async fn converse(
    transport: &dyn Transport,
    server: &HostPort,
    security: Security,
    session: &mut Session,
    limit: Duration,
) -> Result<(), PluginError> {
    let mut io = timed(limit, "the connection", transport.connect(server, security)).await?;
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = vec![0_u8; 4096];
    loop {
        let (reply, used) = loop {
            if let Some(found) = read_reply(&buffer)? {
                break found;
            }
            let read = timed(limit, "in time", async {
                io.read(&mut chunk)
                    .await
                    .map_err(|_| PluginError::unavailable("the connection to the server failed"))
            })
            .await?;
            if read == 0 {
                return Err(PluginError::unavailable("the server closed the connection"));
            }
            buffer.extend_from_slice(&chunk[..read]);
            if buffer.len() > MAX_BUFFERED_BYTES {
                return Err(PluginError::unavailable(
                    "the server sent more than it was asked for",
                ));
            }
        };
        buffer.drain(..used);
        let mut step = session.on_reply(&reply)?;
        if step == Step::StartTls {
            // Anything the server sent after its 220 came in the clear and
            // must not be read as if it came over TLS (RFC 3207 section 6).
            if !buffer.is_empty() {
                return Err(PluginError::unavailable(
                    "the server sent data before TLS started",
                ));
            }
            io = timed(
                limit,
                "the TLS handshake",
                transport.start_tls(&server.hostname, io),
            )
            .await?;
            step = session.tls_started()?;
        }
        let (data, done) = match step {
            Step::Write(data) => (data, false),
            Step::Done(data) => (data, true),
            Step::StartTls => {
                return Err(PluginError::unavailable("STARTTLS was asked for twice"));
            }
        };
        let write = async {
            io.write_all(data.as_bytes()).await?;
            io.flush().await
        };
        let written = timed(limit, "a write", async {
            write
                .await
                .map_err(|_| PluginError::unavailable("the connection to the server failed"))
        })
        .await;
        if done {
            // The message was accepted; a failed QUIT or close changes
            // nothing.
            let _ = io.shutdown().await;
            return Ok(());
        }
        written?;
    }
}

/// The guest's transport: a TCP tunnel through the host, then rustls with
/// Mozilla's root certificates.
#[derive(Debug, Clone, Copy, Default)]
pub struct HostTransport;

#[async_trait(?Send)]
impl Transport for HostTransport {
    async fn connect(&self, server: &HostPort, security: Security) -> Result<BoxIo, PluginError> {
        use bookclerk_plugin_sdk::net::{connect, ConnectOptions, SecureTransport, SocketAddress};
        let socket = connect(
            SocketAddress {
                hostname: server.hostname.clone(),
                port: server.port,
            },
            ConnectOptions {
                secure_transport: match security {
                    Security::StartTls => SecureTransport::StartTls,
                    Security::Tls | Security::None => SecureTransport::Off,
                },
                allow_half_open: false,
            },
        )
        .await
        .map_err(|err| refused(&err.to_string()))?;
        let stream = socket.into_stream();
        if security == Security::Tls {
            return self.start_tls(&server.hostname, Box::new(stream)).await;
        }
        Ok(Box::new(stream))
    }

    async fn start_tls(&self, hostname: &str, io: BoxIo) -> Result<BoxIo, PluginError> {
        let tls = bookclerk_plugin_sdk::http::start_tls(hostname, io)
            .await
            .map_err(|_| {
                PluginError::unavailable(
                    "the TLS handshake failed (the server's certificate must be valid for its name)",
                )
            })?;
        Ok(Box::new(tls))
    }
}

/// Maps a refused tunnel to a failure: one the network grant does not
/// cover is `network_not_granted`, anything else `unavailable`.
///
/// # Arguments
///
/// * `detail` - The SDK's error text, which names only the destination.
fn refused(detail: &str) -> PluginError {
    if detail.contains("403") || detail.contains("network_not_granted") {
        return PluginError::new(
            bookclerk_plugin_sdk::PluginErrorCode::NetworkNotGranted,
            "the host refused the connection to the server; approve its destination in the plugin's network settings",
        );
    }
    PluginError::unavailable("the server could not be reached")
}

#[cfg(test)]
mod tests;
