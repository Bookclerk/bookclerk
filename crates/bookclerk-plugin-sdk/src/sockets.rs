//! Host-brokered WebSockets (`[sockets]`) for native guests.
//!
//! A plugin that declares `[sockets]` (with `capabilities.network.mode =
//! "outbound"`) receives [`Sockets`] on [`Bindings::sockets`](crate::Bindings)
//! and exports a [`SocketHandler`] on
//! [`Entrypoints::socket_handler`](crate::Entrypoints). The host's broker holds
//! each connection, answers declared heartbeats itself and wakes the handler
//! once per message, so the plugin process may sleep between messages: the
//! model of hibernatable Workers WebSockets.
//!
//! [`HostWebSocket`] wraps one socket id with the same surface the
//! TypeScript and Python SDKs give their handlers (`send`, `close`,
//! `serialize_attachment`, `deserialize_attachment`). Attachments are stored
//! as UTF-8 JSON so a socket opened by one SDK reads the same in another.

use serde::de::DeserializeOwned;
use serde::Serialize;

pub use bookclerk_plugin_abi::{
    SocketAutoResponse, SocketClosed, SocketConnectRequest, SocketFrame, SocketHandler,
    SocketHeader, SocketInfo, SocketMessage, SocketMessageOk, Sockets, MAX_SOCKETS_PER_INSTANCE,
    MAX_SOCKET_ATTACHMENT_BYTES, MAX_SOCKET_AUTO_RESPONSES, MAX_SOCKET_AUTO_RESPONSE_BYTES,
    MAX_SOCKET_FRAME_BYTES, MAX_SOCKET_HEADERS, MAX_SOCKET_URL_BYTES,
};

use crate::PluginError;

/// A text heartbeat pair: when `request` arrives the broker sends `response`
/// without waking the guest (for example Socket.io's `"2"` ping and `"3"` pong).
///
/// # Arguments
///
/// * `request` - Text frame to match byte for byte.
/// * `response` - Text frame to send back.
///
/// # Returns
///
/// The pair for [`SocketConnectRequest::auto_responses`].
pub fn text_heartbeat(
    request: impl Into<String>,
    response: impl Into<String>,
) -> SocketAutoResponse {
    SocketAutoResponse {
        request: SocketFrame::Text(request.into()),
        response: SocketFrame::Text(response.into()),
    }
}

/// Encodes an attachment value as the host stores it: UTF-8 JSON.
///
/// # Arguments
///
/// * `value` - Value to keep with the socket.
///
/// # Returns
///
/// At most [`MAX_SOCKET_ATTACHMENT_BYTES`] bytes.
///
/// # Errors
///
/// Returns `invalid_params` when the value does not serialize as JSON and
/// `payload_too_large` when it does not fit.
pub fn encode_attachment<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, PluginError> {
    let bytes = serde_json::to_vec(value).map_err(|err| {
        PluginError::invalid_params(format!("socket attachment is not JSON: {err}"))
    })?;
    let limit = usize::try_from(MAX_SOCKET_ATTACHMENT_BYTES).unwrap_or(usize::MAX);
    if bytes.len() > limit {
        return Err(PluginError::payload_too_large(format!(
            "socket attachment of {} bytes exceeds {MAX_SOCKET_ATTACHMENT_BYTES}",
            bytes.len()
        )));
    }
    Ok(bytes)
}

/// Decodes stored attachment bytes.
///
/// # Arguments
///
/// * `bytes` - Attachment the host kept.
///
/// # Returns
///
/// `None` when no attachment is set.
///
/// # Errors
///
/// Returns `invalid_params` when the bytes are not JSON of type `T`.
pub fn decode_attachment<T: DeserializeOwned>(bytes: &[u8]) -> Result<Option<T>, PluginError> {
    if bytes.is_empty() {
        return Ok(None);
    }
    serde_json::from_slice(bytes).map(Some).map_err(|err| {
        PluginError::invalid_params(format!("socket attachment does not decode: {err}"))
    })
}

/// One host-held socket, named by its id, over the plugin's [`Sockets`] grant.
///
/// The handle holds no connection: it is cheap to build from a
/// [`SocketMessage`] or [`SocketClosed`] in the handler, and stays valid
/// across invocations while the socket is open.
pub struct HostWebSocket<'a> {
    /// The plugin's `SOCKETS` binding.
    sockets: &'a dyn Sockets,
    /// Host-minted socket id.
    id: String,
    /// Attachment as last seen or set through this handle.
    attachment: Vec<u8>,
}

impl<'a> HostWebSocket<'a> {
    /// Opens a socket and wraps it.
    ///
    /// # Arguments
    ///
    /// * `sockets` - The plugin's `SOCKETS` binding.
    /// * `request` - URL, subprotocols, headers, heartbeats and attachment.
    ///
    /// # Returns
    ///
    /// The handle and the host's [`SocketInfo`] for it.
    ///
    /// # Errors
    ///
    /// Returns the host's error: `invalid_params` for a request outside the
    /// ABI rules, `forbidden` for a URL off the network grant,
    /// `limit_exceeded` past [`MAX_SOCKETS_PER_INSTANCE`].
    pub async fn connect(
        sockets: &'a dyn Sockets,
        request: SocketConnectRequest,
    ) -> Result<(Self, SocketInfo), PluginError> {
        let info = sockets.connect(request).await?;
        Ok((Self::from_info(sockets, &info), info))
    }

    /// Wraps a socket `connect` or `list` returned.
    ///
    /// # Arguments
    ///
    /// * `sockets` - The plugin's `SOCKETS` binding.
    /// * `info` - The socket.
    ///
    /// # Returns
    ///
    /// The handle.
    pub fn from_info(sockets: &'a dyn Sockets, info: &SocketInfo) -> Self {
        Self::new(sockets, info.socket.clone(), info.attachment.clone())
    }

    /// Wraps the socket a message arrived on.
    ///
    /// # Arguments
    ///
    /// * `sockets` - The plugin's `SOCKETS` binding.
    /// * `event` - The message.
    ///
    /// # Returns
    ///
    /// The handle, carrying the attachment the message arrived with.
    pub fn from_message(sockets: &'a dyn Sockets, event: &SocketMessage) -> Self {
        Self::new(sockets, event.socket.clone(), event.attachment.clone())
    }

    /// Wraps a socket by id.
    ///
    /// # Arguments
    ///
    /// * `sockets` - The plugin's `SOCKETS` binding.
    /// * `id` - Host-minted socket id.
    /// * `attachment` - Attachment last seen for it.
    ///
    /// # Returns
    ///
    /// The handle.
    pub fn new(sockets: &'a dyn Sockets, id: String, attachment: Vec<u8>) -> Self {
        Self {
            sockets,
            id,
            attachment,
        }
    }

    /// Host-minted socket id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Sends one frame.
    ///
    /// # Errors
    ///
    /// Returns `payload_too_large` past [`MAX_SOCKET_FRAME_BYTES`] and
    /// `not_found` when the socket is closed.
    pub async fn send(&self, frame: SocketFrame) -> Result<(), PluginError> {
        self.sockets.send(self.id.clone(), frame).await
    }

    /// Sends one text frame.
    ///
    /// # Errors
    ///
    /// As [`HostWebSocket::send`].
    pub async fn send_text(&self, text: impl Into<String>) -> Result<(), PluginError> {
        self.send(SocketFrame::Text(text.into())).await
    }

    /// Closes the socket.
    ///
    /// # Arguments
    ///
    /// * `code` - Close code; `0` sends 1000.
    /// * `reason` - Close reason, at most 123 UTF-8 bytes.
    ///
    /// # Errors
    ///
    /// Returns `invalid_params` for a code a peer may not send.
    pub async fn close(&self, code: u16, reason: impl Into<String>) -> Result<(), PluginError> {
        self.sockets
            .close(self.id.clone(), code, reason.into())
            .await
    }

    /// Stores a JSON value with the socket; every later message carries it.
    ///
    /// # Errors
    ///
    /// As [`encode_attachment`], plus the host's `not_found` for a closed
    /// socket.
    pub async fn serialize_attachment<T: Serialize + ?Sized>(
        &mut self,
        value: &T,
    ) -> Result<(), PluginError> {
        let bytes = encode_attachment(value)?;
        self.sockets
            .set_attachment(self.id.clone(), bytes.clone())
            .await?;
        self.attachment = bytes;
        Ok(())
    }

    /// The stored attachment, decoded.
    ///
    /// # Errors
    ///
    /// As [`decode_attachment`].
    pub fn deserialize_attachment<T: DeserializeOwned>(&self) -> Result<Option<T>, PluginError> {
        decode_attachment(&self.attachment)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records every call and answers like a broker with one socket.
    #[derive(Default)]
    struct Recording {
        /// Calls in order, as `op socket detail`.
        calls: RefCell<Vec<String>>,
    }

    #[async_trait::async_trait(?Send)]
    impl Sockets for Recording {
        async fn connect(&self, request: SocketConnectRequest) -> Result<SocketInfo, PluginError> {
            self.calls
                .borrow_mut()
                .push(format!("connect {}", request.url));
            Ok(SocketInfo {
                socket: "g1_a".into(),
                url: request.url,
                protocol: None,
                attachment: request.attachment,
            })
        }
        async fn send(&self, socket: String, frame: SocketFrame) -> Result<(), PluginError> {
            self.calls
                .borrow_mut()
                .push(format!("send {socket} {frame:?}"));
            Ok(())
        }
        async fn close(
            &self,
            socket: String,
            code: u16,
            reason: String,
        ) -> Result<(), PluginError> {
            self.calls
                .borrow_mut()
                .push(format!("close {socket} {code} {reason}"));
            Ok(())
        }
        async fn set_attachment(
            &self,
            socket: String,
            attachment: Vec<u8>,
        ) -> Result<(), PluginError> {
            let text = String::from_utf8(attachment).unwrap();
            self.calls
                .borrow_mut()
                .push(format!("attachment {socket} {text}"));
            Ok(())
        }
        async fn list(&self) -> Result<Vec<SocketInfo>, PluginError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn handle_calls_the_binding_with_its_id() {
        let sockets = Recording::default();
        let request = SocketConnectRequest {
            url: "wss://abs.example/socket.io/".into(),
            auto_responses: vec![text_heartbeat("2", "3")],
            attachment: encode_attachment(&serde_json::json!({ "user": "u1" })).unwrap(),
            ..SocketConnectRequest::default()
        };
        let (mut ws, info) = HostWebSocket::connect(&sockets, request).await.unwrap();
        assert_eq!(info.socket, "g1_a");
        assert_eq!(ws.id(), "g1_a");
        let user: serde_json::Value = ws.deserialize_attachment().unwrap().unwrap();
        assert_eq!(user["user"], "u1");
        ws.send_text("40").await.unwrap();
        ws.serialize_attachment(&7_u32).await.unwrap();
        assert_eq!(ws.deserialize_attachment::<u32>().unwrap(), Some(7));
        ws.close(1000, "bye").await.unwrap();
        assert_eq!(
            *sockets.calls.borrow(),
            vec![
                "connect wss://abs.example/socket.io/".to_string(),
                "send g1_a Text(\"40\")".into(),
                "attachment g1_a 7".into(),
                "close g1_a 1000 bye".into(),
            ]
        );
        let event = SocketMessage {
            socket: "g1_b".into(),
            frame: SocketFrame::Text("x".into()),
            attachment: Vec::new(),
        };
        let other = HostWebSocket::from_message(&sockets, &event);
        assert_eq!(other.id(), "g1_b");
        assert_eq!(other.deserialize_attachment::<u32>().unwrap(), None);
    }

    #[test]
    fn attachments_are_bounded_json() {
        assert_eq!(
            encode_attachment(&serde_json::json!(null)).unwrap(),
            b"null"
        );
        let big = "x".repeat(usize::try_from(MAX_SOCKET_ATTACHMENT_BYTES).unwrap());
        let err = encode_attachment(&big).unwrap_err();
        assert_eq!(err.code, crate::PluginErrorCode::PayloadTooLarge);
        let err = decode_attachment::<u32>(b"not json").unwrap_err();
        assert_eq!(err.code, crate::PluginErrorCode::InvalidParams);
        let mut map = std::collections::HashMap::new();
        map.insert(vec![1_u8], 1_u8);
        let err = encode_attachment(&map).unwrap_err();
        assert_eq!(err.code, crate::PluginErrorCode::InvalidParams);
    }
}
