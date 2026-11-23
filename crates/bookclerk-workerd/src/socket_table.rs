//! The socket broker's bookkeeping, without I/O.
//!
//! [`SocketTable`] holds every socket one plugin instance has open through
//! the broker: its id, URL, negotiated subprotocol, attachment and declared
//! heartbeat pairs. The broker's network code (`socket_broker`) asks it what
//! to do with each frame the remote side sends and each request the guest
//! makes, so the decisions the fuzz target checks are the ones the broker
//! acts on.
//!
//! A socket is *pending* from [`SocketTable::reserve`] until the handshake
//! finishes ([`SocketTable::establish`]) or fails
//! ([`SocketTable::abandon`]). Pending sockets count against
//! [`MAX_SOCKETS_PER_INSTANCE`] but are invisible to the guest. Ids are never
//! reused within one table.

use bookclerk_plugin_abi::sockets::{
    auto_response_for, check_socket_attachment, check_socket_close, check_socket_frame_for,
    socket_frame_len, validate_socket_connect, SocketScheme, SocketTarget,
};
use bookclerk_plugin_abi::{
    LimitExceeded, PluginError, Result, SocketAutoResponse, SocketClosed, SocketConnectRequest,
    SocketFrame, SocketInfo, SocketMessage, MAX_SOCKETS_PER_INSTANCE, MAX_SOCKET_FRAME_BYTES,
};

/// WebSocket close code for a message larger than the broker accepts (RFC
/// 6455 "Message Too Big").
pub const CLOSE_TOO_BIG: u16 = 1009;

/// Longest id prefix [`SocketTable::new`] keeps, so ids stay within the
/// 64-byte id limit with any counter value.
pub const MAX_ID_PREFIX_BYTES: usize = 32;

/// Whether a socket's handshake has finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Dialing or in the handshake; not listed and not addressable.
    Pending,
    /// Open: listed, and frames flow both ways.
    Open,
}

/// One socket the table tracks.
#[derive(Debug, Clone)]
struct Entry {
    /// Socket id handed to the guest.
    id: String,
    /// URL the guest asked for, as given.
    url: String,
    /// WebSocket or `tls://` stream; a stream takes binary frames only.
    scheme: SocketScheme,
    /// Subprotocol the server chose; `None` before the handshake or when
    /// it chose none.
    protocol: Option<String>,
    /// Opaque bytes the guest keeps with the socket, at most
    /// `MAX_SOCKET_ATTACHMENT_BYTES`.
    attachment: Vec<u8>,
    /// Heartbeat pairs the broker answers itself.
    auto_responses: Vec<SocketAutoResponse>,
    /// Handshake state.
    phase: Phase,
}

impl Entry {
    /// The guest-facing description of this socket.
    fn info(&self) -> SocketInfo {
        SocketInfo {
            socket: self.id.clone(),
            url: self.url.clone(),
            protocol: self.protocol.clone(),
            attachment: self.attachment.clone(),
        }
    }
}

/// What the broker does with one data frame the remote side sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inbound {
    /// A declared heartbeat: send this frame back and do not wake the guest.
    Reply(SocketFrame),
    /// Wake the guest and hand it this message.
    Deliver(SocketMessage),
    /// The frame is over `MAX_SOCKET_FRAME_BYTES`: close the socket with
    /// [`CLOSE_TOO_BIG`].
    TooBig,
    /// The socket is not open (closed, or never known): drop the frame.
    Ignore,
}

/// Open and pending sockets for one plugin instance.
#[derive(Debug, Clone)]
pub struct SocketTable {
    /// Prefix of every id this table hands out.
    prefix: String,
    /// Counter for the next id; never reset, so ids are not reused.
    next: u64,
    /// Sockets, oldest first.
    entries: Vec<Entry>,
}

impl SocketTable {
    /// An empty table whose ids start with `prefix`.
    ///
    /// # Arguments
    ///
    /// * `prefix` - Id prefix; characters outside `[A-Za-z0-9_-]` are
    ///   dropped and it is cut to [`MAX_ID_PREFIX_BYTES`], so every id passes
    ///   `check_socket_id`.
    #[must_use]
    pub fn new(prefix: &str) -> Self {
        let prefix: String = prefix
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .take(MAX_ID_PREFIX_BYTES)
            .collect();
        Self {
            prefix,
            next: 0,
            entries: Vec::new(),
        }
    }

    /// Sockets counted against the per-instance limit, pending included.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the table holds no socket, pending or open.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Index of the entry `socket` names, in any phase.
    fn position(&self, socket: &str) -> Option<usize> {
        self.entries.iter().position(|e| e.id == socket)
    }

    /// The open entry `socket` names.
    fn open_mut(&mut self, socket: &str) -> Option<&mut Entry> {
        self.entries
            .iter_mut()
            .find(|e| e.id == socket && e.phase == Phase::Open)
    }

    /// Validates a connect request and reserves a pending socket for it.
    ///
    /// # Returns
    ///
    /// The new socket id and the parsed target to dial.
    ///
    /// # Errors
    ///
    /// The `validate_socket_connect` refusal, or `limit_exceeded` when the
    /// instance already holds `MAX_SOCKETS_PER_INSTANCE` sockets.
    pub fn reserve(&mut self, request: &SocketConnectRequest) -> Result<(String, SocketTarget)> {
        let target = validate_socket_connect(request)?;
        let cap = MAX_SOCKETS_PER_INSTANCE as usize;
        if self.entries.len() >= cap {
            return Err(PluginError::limit_exceeded(LimitExceeded::new(
                "maxSocketsPerInstance",
                u64::from(MAX_SOCKETS_PER_INSTANCE),
                u64::try_from(self.entries.len() + 1).unwrap_or(u64::MAX),
            )));
        }
        let id = format!("{}{}", self.prefix, self.next);
        self.next = self.next.saturating_add(1);
        self.entries.push(Entry {
            id: id.clone(),
            url: request.url.clone(),
            scheme: target.scheme,
            protocol: None,
            attachment: request.attachment.clone(),
            auto_responses: request.auto_responses.clone(),
            phase: Phase::Pending,
        });
        Ok((id, target))
    }

    /// Marks a pending socket open once its handshake finished.
    ///
    /// # Arguments
    ///
    /// * `socket` - Id from [`Self::reserve`].
    /// * `protocol` - Subprotocol the server chose, if any.
    ///
    /// # Returns
    ///
    /// The socket's description for the guest, or `None` when `socket` is
    /// not pending (already open, abandoned, or unknown).
    pub fn establish(&mut self, socket: &str, protocol: Option<String>) -> Option<SocketInfo> {
        let entry = self
            .entries
            .iter_mut()
            .find(|e| e.id == socket && e.phase == Phase::Pending)?;
        entry.phase = Phase::Open;
        entry.protocol = protocol;
        Some(entry.info())
    }

    /// Drops a pending socket whose dial or handshake failed.
    ///
    /// # Returns
    ///
    /// True when a pending socket was removed; open sockets are left alone
    /// (they end through [`Self::closed`]).
    pub fn abandon(&mut self, socket: &str) -> bool {
        match self.position(socket) {
            Some(index) if self.entries[index].phase == Phase::Pending => {
                self.entries.remove(index);
                true
            }
            _ => false,
        }
    }

    /// Decides what to do with a data frame the remote side sent.
    #[must_use]
    pub fn inbound(&self, socket: &str, frame: SocketFrame) -> Inbound {
        let Some(entry) = self
            .entries
            .iter()
            .find(|e| e.id == socket && e.phase == Phase::Open)
        else {
            return Inbound::Ignore;
        };
        if socket_frame_len(&frame) > MAX_SOCKET_FRAME_BYTES as usize {
            return Inbound::TooBig;
        }
        if let Some(reply) = auto_response_for(&frame, &entry.auto_responses) {
            return Inbound::Reply(reply.clone());
        }
        Inbound::Deliver(SocketMessage {
            socket: entry.id.clone(),
            frame,
            attachment: entry.attachment.clone(),
        })
    }

    /// Checks a frame the guest wants to send.
    ///
    /// # Errors
    ///
    /// `not_found` when the socket is not open, or the frame check's
    /// refusal.
    pub fn outbound(&self, socket: &str, frame: &SocketFrame) -> Result<()> {
        let Some(entry) = self
            .entries
            .iter()
            .find(|e| e.id == socket && e.phase == Phase::Open)
        else {
            return Err(PluginError::not_found(format!(
                "socket `{socket}` is not open"
            )));
        };
        check_socket_frame_for(entry.scheme, frame)
    }

    /// Checks a close the guest asked for.
    ///
    /// # Returns
    ///
    /// The close code to send (zero becomes 1000), or `None` when the
    /// socket is not open, which is not an error.
    ///
    /// # Errors
    ///
    /// The close code or reason check's refusal.
    pub fn close_request(&self, socket: &str, code: u16, reason: &str) -> Result<Option<u16>> {
        let code = check_socket_close(code, reason)?;
        Ok(self
            .entries
            .iter()
            .any(|e| e.id == socket && e.phase == Phase::Open)
            .then_some(code))
    }

    /// Replaces an open socket's attachment.
    ///
    /// # Errors
    ///
    /// The attachment check's refusal, or `not_found` when the socket is
    /// not open.
    pub fn set_attachment(&mut self, socket: &str, attachment: Vec<u8>) -> Result<()> {
        check_socket_attachment(&attachment)?;
        let entry = self
            .open_mut(socket)
            .ok_or_else(|| PluginError::not_found(format!("socket `{socket}` is not open")))?;
        entry.attachment = attachment;
        Ok(())
    }

    /// Records that an open socket ended and removes it.
    ///
    /// # Arguments
    ///
    /// * `socket` - The socket that ended.
    /// * `code` - Close code; 1006 when the connection dropped without one.
    /// * `reason` - Close reason the peer sent; empty for none.
    /// * `clean` - True when the close handshake completed.
    ///
    /// # Returns
    ///
    /// The event for the guest, carrying the last attachment, or `None`
    /// when the socket was not open, so a socket is reported closed once.
    pub fn closed(
        &mut self,
        socket: &str,
        code: u16,
        reason: &str,
        clean: bool,
    ) -> Option<SocketClosed> {
        let index = self.position(socket)?;
        if self.entries[index].phase != Phase::Open {
            return None;
        }
        let entry = self.entries.remove(index);
        Some(SocketClosed {
            socket: entry.id,
            code,
            reason: (!reason.is_empty()).then(|| reason.to_string()),
            clean,
            attachment: entry.attachment,
        })
    }

    /// The open sockets, oldest first.
    #[must_use]
    pub fn list(&self) -> Vec<SocketInfo> {
        self.entries
            .iter()
            .filter(|e| e.phase == Phase::Open)
            .map(Entry::info)
            .collect()
    }
}

#[cfg(test)]
#[path = "socket_table_tests.rs"]
mod tests;
