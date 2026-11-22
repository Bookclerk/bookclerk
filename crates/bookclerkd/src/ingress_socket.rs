//! WebSocket upgrades a relay plugin carries (`Origin.connect`).
//!
//! An upgrade passes the same tunnel rules as a plain request ([`gate`]).
//! The router can only upgrade a request that came in on a real HTTP
//! connection, so the daemon serves it one: an in-memory duplex with the
//! router on its server end and a client handshake on the other, both on
//! the daemon's runtime. Once the router answers `101`, the client end is a
//! WebSocket the plugin host holds for the plugin instance; frames cross
//! between them through the [`LocalSocket`] channels. Any other answer goes
//! back to the plugin as an [`OriginRefusal`].

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use axum::body::Body;
use axum::extract::connect_info::ConnectInfo;
use axum::http::{header, HeaderValue, Request, StatusCode};
use bookclerk_plugin_abi::origin::CheckedOriginRequest;
use bookclerk_plugin_abi::{
    OriginHeader, OriginRefusal, PluginError, SocketFrame, MAX_ORIGIN_REFUSAL_BODY_BYTES,
    MAX_SOCKET_FRAME_BYTES,
};
use bookclerk_plugin_host::{LocalInbound, LocalOutbound, LocalSocket, OriginUpgrade};
use futures::{SinkExt as _, StreamExt as _};
use hyper_util::rt::TokioIo;
use tokio::io::AsyncReadExt as _;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::WebSocketStream;
use tower_service::Service as _;

use super::{answer, gate, refused, tunnel_request, Gate, Shared, TunnelAnswer};
use crate::node_api::transport::ListenerKind;

/// Bytes the in-memory connection buffers each way.
const DUPLEX_BYTES: usize = 64 * 1024;

/// Messages buffered each way between the WebSocket and the plugin host.
/// The daemon stops reading the socket while the plugin is behind.
const FRAME_CHANNEL: usize = 16;

/// Longest the router may take to answer the upgrade request.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest a close waits for the library's close reply.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// WebSocket close code for a message larger than a plugin may receive
/// (RFC 6455 "Message Too Big").
const CLOSE_TOO_BIG: u16 = 1009;

/// WebSocket close code when the connection dropped without a close.
const CLOSE_ABNORMAL: u16 = 1006;

/// WebSocket close code sent when the plugin instance is gone.
const CLOSE_GOING_AWAY: u16 = 1001;

/// Opens a WebSocket to the library for one checked upgrade request.
///
/// # Errors
///
/// `unavailable` while the daemon cannot serve or the in-memory connection
/// fails; `invalid_params` for a head the router could not take.
pub(super) async fn open(
    shared: &Shared,
    request: CheckedOriginRequest,
    client: IpAddr,
) -> bookclerk_plugin_abi::Result<OriginUpgrade> {
    let (router, authority) = match gate(shared, &request).await? {
        Gate::Refused(refusal) => {
            return Ok(OriginUpgrade::Refused(refusal_of(refused(refusal)).await))
        }
        Gate::Open { router, authority } => (router, authority),
    };
    let path = request.target.split('?').next().unwrap_or("/").to_string();
    let mut http = tunnel_request(request, &authority, client, None)?;
    let key = tokio_tungstenite::tungstenite::handshake::client::generate_key();
    let headers = http.headers_mut();
    headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    headers.insert(
        header::SEC_WEBSOCKET_VERSION,
        HeaderValue::from_static("13"),
    );
    headers.insert(
        header::SEC_WEBSOCKET_KEY,
        HeaderValue::from_str(&key)
            .map_err(|_| PluginError::internal("the WebSocket key is not a header value"))?,
    );
    // The extensions stay on this side of the connection; the server end
    // sets them again on the request the router sees.
    http.extensions_mut().clear();

    let (client_io, server_io) = tokio::io::duplex(DUPLEX_BYTES);
    tokio::spawn(serve_router(router, server_io, client));
    let response = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(TokioIo::new(client_io))
                .await
                .map_err(unreachable_library)?;
        tokio::spawn(async move {
            if let Err(err) = connection.with_upgrades().await {
                tracing::debug!(error = %err, "tunnel upgrade connection ended");
            }
        });
        sender.send_request(http).await.map_err(unreachable_library)
    })
    .await
    .map_err(|_| PluginError::unavailable("the library did not answer the upgrade in time"))??;

    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        let response = response.map(Body::new);
        return Ok(OriginUpgrade::Refused(
            refusal_of(answer(&shared.runtime, response)).await,
        ));
    }
    let accepted = response
        .headers()
        .get(header::SEC_WEBSOCKET_ACCEPT)
        .and_then(|value| value.to_str().ok());
    if accepted != Some(derive_accept_key(key.as_bytes()).as_str()) {
        return Err(PluginError::unavailable(
            "the library's upgrade answer did not match the key",
        ));
    }
    let protocol = response
        .headers()
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let upgraded = hyper::upgrade::on(response)
        .await
        .map_err(unreachable_library)?;
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(MAX_SOCKET_FRAME_BYTES as usize);
    config.max_frame_size = Some(MAX_SOCKET_FRAME_BYTES as usize);
    let socket =
        WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Client, Some(config)).await;
    let (outbound_tx, outbound_rx) = mpsc::channel(FRAME_CHANNEL);
    let (inbound_tx, inbound_rx) = mpsc::channel(FRAME_CHANNEL);
    tokio::spawn(pump(socket, outbound_rx, inbound_tx));
    Ok(OriginUpgrade::Accepted(LocalSocket {
        runtime: shared.runtime.clone(),
        url: format!("https://{authority}{path}"),
        protocol,
        outbound: outbound_tx,
        inbound: inbound_rx,
    }))
}

/// The plugin's error for a failure of the in-memory connection.
fn unreachable_library(err: hyper::Error) -> PluginError {
    PluginError::unavailable(format!("the library's upgrade connection failed: {err}"))
}

/// Serves the router on the server end of one in-memory connection, with
/// the tunnel's peer and listener on every request.
async fn serve_router(router: axum::Router, io: tokio::io::DuplexStream, client: IpAddr) {
    let service = hyper::service::service_fn(move |mut request: Request<hyper::body::Incoming>| {
        let extensions = request.extensions_mut();
        extensions.insert(ConnectInfo(SocketAddr::new(client, 0)));
        extensions.insert(ListenerKind::Tunnel);
        let mut router = router.clone();
        async move { router.call(request.map(Body::new)).await }
    });
    if let Err(err) = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(io), service)
        .with_upgrades()
        .await
    {
        tracing::debug!(error = %err, "tunnel upgrade server connection ended");
    }
}

/// The library's answer as a refusal: its head as the tunnel passes it
/// on, and its body cut to [`MAX_ORIGIN_REFUSAL_BODY_BYTES`].
async fn refusal_of(answer: TunnelAnswer) -> OriginRefusal {
    let mut body = Vec::new();
    if let Some(reader) = answer.body {
        let limit = u64::from(MAX_ORIGIN_REFUSAL_BODY_BYTES);
        if let Err(err) = reader.take(limit).read_to_end(&mut body).await {
            tracing::debug!(error = %err, "tunnel refusal body ended early");
        }
    }
    let mut headers: Vec<OriginHeader> = answer.headers;
    // The body may be cut, so its length no longer holds.
    headers.retain(|field| !field.name.eq_ignore_ascii_case("content-length"));
    OriginRefusal {
        // An interim answer is not one a client can take; it reads as the
        // library failing.
        status: if answer.status < 200 {
            StatusCode::BAD_GATEWAY.as_u16()
        } else {
            answer.status
        },
        headers,
        body,
    }
}

/// Carries frames between the library's WebSocket and the plugin host
/// until either side closes.
///
/// The close reaches the plugin host last, once. A close the plugin asked
/// for waits up to [`CLOSE_TIMEOUT`] for the library's reply. The plugin
/// host dropping its sender (the instance is gone) closes the socket with
/// [`CLOSE_GOING_AWAY`] and reports nothing.
async fn pump<S>(
    mut socket: WebSocketStream<S>,
    mut outbound: mpsc::Receiver<LocalOutbound>,
    inbound: mpsc::Sender<LocalInbound>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let closed = loop {
        tokio::select! {
            message = socket.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    let frame = SocketFrame::Text(text.as_str().to_string());
                    if inbound.send(LocalInbound::Frame(frame)).await.is_err() {
                        let _ = close(&mut socket, CLOSE_GOING_AWAY, "").await;
                        return;
                    }
                }
                Some(Ok(Message::Binary(bytes))) => {
                    let frame = SocketFrame::Binary(bytes.to_vec());
                    if inbound.send(LocalInbound::Frame(frame)).await.is_err() {
                        let _ = close(&mut socket, CLOSE_GOING_AWAY, "").await;
                        return;
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                Some(Ok(Message::Close(frame))) => {
                    // Answer the library's close before reporting it.
                    let _ = socket.flush().await;
                    break LocalInbound::Closed {
                        code: frame.as_ref().map_or(1005, |f| u16::from(f.code)),
                        reason: frame
                            .map(|f| f.reason.as_str().to_string())
                            .filter(|reason| !reason.is_empty()),
                        clean: true,
                    };
                }
                Some(Err(WsError::Capacity(_))) => {
                    let _ = close(&mut socket, CLOSE_TOO_BIG, "message too big").await;
                    break LocalInbound::Closed {
                        code: CLOSE_TOO_BIG,
                        reason: Some("message too big".into()),
                        clean: false,
                    };
                }
                Some(Err(_)) | None => break LocalInbound::Closed {
                    code: CLOSE_ABNORMAL,
                    reason: None,
                    clean: false,
                },
            },
            request = outbound.recv() => match request {
                Some(LocalOutbound::Frame(frame)) => {
                    let message = match frame {
                        SocketFrame::Text(text) => Message::text(text),
                        SocketFrame::Binary(bytes) => Message::binary(bytes),
                    };
                    if socket.send(message).await.is_err() {
                        break LocalInbound::Closed {
                            code: CLOSE_ABNORMAL,
                            reason: None,
                            clean: false,
                        };
                    }
                }
                Some(LocalOutbound::Close { code, reason }) => {
                    let clean = close(&mut socket, code, &reason).await;
                    break LocalInbound::Closed {
                        code,
                        reason: Some(reason).filter(|reason| !reason.is_empty()),
                        clean,
                    };
                }
                None => {
                    let _ = close(&mut socket, CLOSE_GOING_AWAY, "").await;
                    return;
                }
            },
        }
    };
    let _ = inbound.send(closed).await;
}

/// Sends a close and waits for the library's reply.
///
/// # Returns
///
/// Whether the close handshake completed within [`CLOSE_TIMEOUT`].
async fn close<S>(socket: &mut WebSocketStream<S>, code: u16, reason: &str) -> bool
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let frame = CloseFrame {
        code: CloseCode::from(code),
        reason: reason.to_string().into(),
    };
    tokio::time::timeout(CLOSE_TIMEOUT, async {
        if socket.close(Some(frame)).await.is_err() {
            return false;
        }
        loop {
            match socket.next().await {
                // The library's reply ends the stream (or arrives as a
                // last message).
                Some(Ok(Message::Close(_)) | Err(WsError::ConnectionClosed)) | None => return true,
                Some(Ok(_)) => {}
                Some(Err(_)) => return false,
            }
        }
    })
    .await
    .unwrap_or(false)
}
