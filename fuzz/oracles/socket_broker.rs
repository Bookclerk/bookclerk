//! Shared oracle for the `socket_broker` fuzz target and the
//! `bookclerk-workerd` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is a script of operations on the socket broker's bookkeeping
//! ([`SocketTable`]): connect requests and frames a guest sends, frames and
//! closes a remote WebSocket server sends, and handshake outcomes. Every
//! operation is checked against an independent model of what the broker
//! must do:
//!
//! - no operation panics, and the table never holds more than
//!   `MAX_SOCKETS_PER_INSTANCE` sockets, pending included;
//! - ids are valid, unique, and never reused after a socket ends;
//! - a frame from the server is answered by the broker exactly when it is
//!   equal (same kind, same bytes) to a declared heartbeat, with that
//!   pair's response, so heartbeats never wake the guest; it closes the
//!   socket with 1009 exactly when it is over `MAX_SOCKET_FRAME_BYTES`;
//!   otherwise it is delivered with the socket's current attachment;
//! - nothing reaches the guest for a pending or ended socket, and a socket
//!   is reported closed exactly once, with its last attachment;
//! - the listing is the open sockets, oldest first, as the model has them.
//!
//! Script encoding (each op starts with one byte, taken modulo 8; reads
//! past the end yield zero):
//!
//! - `0` reserve: URL choice, pair count (mod 5), each pair as request
//!   then response frame (kind byte, length byte, bytes), then attachment
//!   length byte (times 16);
//! - `1` establish: slot, protocol flag;
//! - `2` abandon: slot;
//! - `3` inbound: slot, kind, length (u16 LE), huge flag (odd means one
//!   byte over the frame cap), then the bytes;
//! - `4` set attachment: slot, length (u16 LE, mod 4097), fill byte;
//! - `5` closed: slot, code (u16 LE), reason length byte;
//! - `6` outbound: slot, frame (kind byte, length byte, bytes);
//! - `7` close request: slot, code (u16 LE), reason length byte.
//!
//! A slot picks one of the ids issued so far, or one never issued.

use bookclerk_plugin_abi::sockets::{check_socket_id, validate_socket_connect};
use bookclerk_plugin_abi::{
    SocketAutoResponse, SocketConnectRequest, SocketFrame, MAX_SOCKETS_PER_INSTANCE,
    MAX_SOCKET_ATTACHMENT_BYTES, MAX_SOCKET_FRAME_BYTES,
};
use bookclerk_workerd::socket_table::{Inbound, SocketTable, CLOSE_TOO_BIG};

/// Largest script worth running; scripts past this are skipped.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;

/// Most operations one script runs.
const MAX_OPS: usize = 256;

/// URLs a reserve op chooses from: two the broker accepts, two it refuses.
const URLS: [&str; 4] = [
    "wss://abs.example.com/socket.io/?EIO=4&transport=websocket",
    "ws://10.0.0.5:13378/socket.io",
    "https://abs.example.com/",
    "ws://user@abs.example.com/",
];

/// The id slot used for "never issued".
const UNKNOWN_ID: &str = "never-issued";

/// Reads script bytes, yielding zero past the end.
struct Script<'a> {
    /// Remaining bytes.
    bytes: &'a [u8],
}

impl Script<'_> {
    /// True when no byte is left.
    fn done(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The next byte, or zero.
    fn byte(&mut self) -> u8 {
        match self.bytes.split_first() {
            Some((&first, rest)) => {
                self.bytes = rest;
                first
            }
            None => 0,
        }
    }

    /// The next little-endian `u16`.
    fn u16(&mut self) -> u16 {
        u16::from_le_bytes([self.byte(), self.byte()])
    }

    /// Up to `len` next bytes (fewer when the script ends).
    fn take(&mut self, len: usize) -> Vec<u8> {
        let len = len.min(self.bytes.len());
        let (head, rest) = self.bytes.split_at(len);
        self.bytes = rest;
        head.to_vec()
    }

    /// A frame: kind byte (even text, odd binary), length byte, bytes.
    fn frame(&mut self) -> SocketFrame {
        let kind = self.byte();
        let len = usize::from(self.byte());
        let bytes = self.take(len);
        make_frame(kind, bytes)
    }
}

/// A text frame for an even `kind`, else binary.
fn make_frame(kind: u8, bytes: Vec<u8>) -> SocketFrame {
    if kind.is_multiple_of(2) {
        SocketFrame::Text(String::from_utf8_lossy(&bytes).into_owned())
    } else {
        SocketFrame::Binary(bytes)
    }
}

/// The model's view of one socket.
#[derive(Debug, Clone)]
struct Model {
    /// Socket id.
    id: String,
    /// URL as requested.
    url: String,
    /// True until the handshake finished.
    pending: bool,
    /// Subprotocol once open.
    protocol: Option<String>,
    /// Current attachment.
    attachment: Vec<u8>,
    /// Declared heartbeat pairs.
    pairs: Vec<SocketAutoResponse>,
}

/// The model and the table under test.
struct Run {
    /// Table under test.
    table: SocketTable,
    /// Live sockets, oldest first.
    live: Vec<Model>,
    /// Every id ever issued, in order.
    issued: Vec<String>,
}

impl Run {
    /// The id a slot byte names.
    fn slot(&self, byte: u8) -> String {
        let index = usize::from(byte) % (self.issued.len() + 1);
        self.issued
            .get(index)
            .cloned()
            .unwrap_or_else(|| UNKNOWN_ID.to_string())
    }

    /// The live model entry for `id`.
    fn live(&self, id: &str) -> Option<&Model> {
        self.live.iter().find(|m| m.id == id)
    }

    /// True when `id` names an open socket.
    fn open(&self, id: &str) -> bool {
        self.live(id).is_some_and(|m| !m.pending)
    }

    /// Op 0.
    fn reserve(&mut self, script: &mut Script<'_>) {
        let url = URLS[usize::from(script.byte()) % URLS.len()].to_string();
        let pairs = (0..script.byte() % 5)
            .map(|_| SocketAutoResponse {
                request: script.frame(),
                response: script.frame(),
            })
            .collect();
        let attachment = vec![0xa5; usize::from(script.byte()) * 16];
        let request = SocketConnectRequest {
            url,
            auto_responses: pairs,
            attachment,
            ..SocketConnectRequest::default()
        };
        let valid = validate_socket_connect(&request).is_ok();
        let room = self.live.len() < MAX_SOCKETS_PER_INSTANCE as usize;
        match self.table.reserve(&request) {
            Ok((id, _target)) => {
                assert!(valid && room, "reserve accepted: valid={valid} room={room}");
                assert!(check_socket_id(&id).is_ok(), "invalid id {id:?}");
                assert!(!self.issued.contains(&id), "id {id:?} reused");
                assert_ne!(id, UNKNOWN_ID);
                self.issued.push(id.clone());
                self.live.push(Model {
                    id,
                    url: request.url,
                    pending: true,
                    protocol: None,
                    attachment: request.attachment,
                    pairs: request.auto_responses,
                });
            }
            Err(_) => assert!(
                !(valid && room),
                "reserve refused a valid request with room"
            ),
        }
    }

    /// Op 1.
    fn establish(&mut self, script: &mut Script<'_>) {
        let id = self.slot(script.byte());
        let protocol = (!script.byte().is_multiple_of(2)).then(|| "chat".to_string());
        let pending = self.live(&id).is_some_and(|m| m.pending);
        let info = self.table.establish(&id, protocol.clone());
        assert_eq!(info.is_some(), pending, "establish {id:?}");
        if let Some(info) = info {
            let model = self
                .live
                .iter_mut()
                .find(|m| m.id == id)
                .unwrap_or_else(|| unreachable!());
            model.pending = false;
            model.protocol = protocol;
            assert_eq!(info.socket, model.id);
            assert_eq!(info.url, model.url);
            assert_eq!(info.protocol, model.protocol);
            assert_eq!(info.attachment, model.attachment);
        }
    }

    /// Op 2.
    fn abandon(&mut self, script: &mut Script<'_>) {
        let id = self.slot(script.byte());
        let pending = self.live(&id).is_some_and(|m| m.pending);
        assert_eq!(self.table.abandon(&id), pending, "abandon {id:?}");
        if pending {
            self.live.retain(|m| m.id != id);
        }
    }

    /// Op 3: a frame from the server.
    fn inbound(&mut self, script: &mut Script<'_>) {
        let id = self.slot(script.byte());
        let kind = script.byte();
        let len = usize::from(script.u16());
        let huge = !script.byte().is_multiple_of(2);
        let bytes = if huge {
            vec![b'x'; MAX_SOCKET_FRAME_BYTES as usize + 1]
        } else {
            script.take(len)
        };
        let frame = make_frame(kind, bytes);
        let frame_len = match &frame {
            SocketFrame::Text(text) => text.len(),
            SocketFrame::Binary(bytes) => bytes.len(),
        };
        let expected = match self.live(&id) {
            Some(model) if !model.pending => {
                if frame_len > MAX_SOCKET_FRAME_BYTES as usize {
                    "too big".to_string()
                } else if let Some(pair) = model.pairs.iter().find(|p| p.request == frame) {
                    format!("reply {:?}", pair.response)
                } else {
                    format!("deliver {:?}", model.attachment)
                }
            }
            _ => "ignore".to_string(),
        };
        let actual = match self.table.inbound(&id, frame.clone()) {
            Inbound::Reply(reply) => format!("reply {reply:?}"),
            Inbound::Deliver(message) => {
                assert_eq!(message.socket, id);
                assert_eq!(message.frame, frame, "delivered frame changed");
                format!("deliver {:?}", message.attachment)
            }
            Inbound::TooBig => "too big".to_string(),
            Inbound::Ignore => "ignore".to_string(),
        };
        assert_eq!(actual, expected, "inbound on {id:?}");
        if actual == "too big" {
            // The broker closes the socket with 1009 and reports it.
            let event = self
                .table
                .closed(&id, CLOSE_TOO_BIG, "message too big", false);
            assert!(event.is_some_and(|e| e.code == CLOSE_TOO_BIG));
            self.live.retain(|m| m.id != id);
        }
    }

    /// Op 4.
    fn set_attachment(&mut self, script: &mut Script<'_>) {
        let id = self.slot(script.byte());
        let len = usize::from(script.u16()) % 4097;
        let attachment = vec![script.byte(); len];
        let ok = self.open(&id) && len <= MAX_SOCKET_ATTACHMENT_BYTES as usize;
        assert_eq!(
            self.table.set_attachment(&id, attachment.clone()).is_ok(),
            ok,
            "set_attachment {id:?} len {len}"
        );
        if ok {
            if let Some(model) = self.live.iter_mut().find(|m| m.id == id) {
                model.attachment = attachment;
            }
        }
    }

    /// Op 5: the socket ended.
    fn closed(&mut self, script: &mut Script<'_>) {
        let id = self.slot(script.byte());
        let code = script.u16();
        let reason = "r".repeat(usize::from(script.byte()));
        let model = self.live(&id).filter(|m| !m.pending).cloned();
        let event = self.table.closed(&id, code, &reason, code == 1000);
        match (event, model) {
            (Some(event), Some(model)) => {
                assert_eq!(event.socket, id);
                assert_eq!(event.code, code);
                assert_eq!(event.reason.unwrap_or_default(), reason);
                assert_eq!(event.attachment, model.attachment, "closed attachment");
                self.live.retain(|m| m.id != id);
            }
            (None, None) => {}
            (event, model) => panic!("closed {id:?}: event={event:?} model={model:?}"),
        }
    }

    /// Op 6.
    fn outbound(&mut self, script: &mut Script<'_>) {
        let id = self.slot(script.byte());
        let frame = script.frame();
        assert_eq!(
            self.table.outbound(&id, &frame).is_ok(),
            self.open(&id),
            "outbound {id:?}"
        );
    }

    /// Op 7.
    fn close_request(&mut self, script: &mut Script<'_>) {
        let id = self.slot(script.byte());
        let code = script.u16();
        let reason = "r".repeat(usize::from(script.byte()));
        let sent_code = if code == 0 { 1000 } else { code };
        // RFC 6455 codes an endpoint may send.
        let code_ok = matches!(sent_code, 1000..=1003 | 1007..=1014 | 3000..=4999);
        let ok = code_ok && reason.len() <= 123;
        match self.table.close_request(&id, code, &reason) {
            Ok(sent) => {
                assert!(ok, "close {code} {} accepted", reason.len());
                let expected = self.open(&id).then_some(sent_code);
                assert_eq!(sent, expected, "close {id:?}");
            }
            Err(_) => assert!(!ok, "close {code} {} refused", reason.len()),
        }
    }

    /// Checks the table against the model after every op.
    fn check_state(&self) {
        assert!(self.table.len() <= MAX_SOCKETS_PER_INSTANCE as usize);
        assert_eq!(self.table.len(), self.live.len(), "socket count");
        let listed: Vec<(String, Option<String>, Vec<u8>)> = self
            .table
            .list()
            .into_iter()
            .map(|info| (info.socket, info.protocol, info.attachment))
            .collect();
        let expected: Vec<(String, Option<String>, Vec<u8>)> = self
            .live
            .iter()
            .filter(|m| !m.pending)
            .map(|m| (m.id.clone(), m.protocol.clone(), m.attachment.clone()))
            .collect();
        assert_eq!(listed, expected, "listing");
    }
}

/// Runs one script through the oracle.
///
/// # Panics
///
/// When an invariant in the module docs breaks.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let mut script = Script { bytes: data };
    let mut run = Run {
        table: SocketTable::new("s-"),
        live: Vec::new(),
        issued: Vec::new(),
    };
    for _ in 0..MAX_OPS {
        if script.done() {
            break;
        }
        match script.byte() % 8 {
            0 => run.reserve(&mut script),
            1 => run.establish(&mut script),
            2 => run.abandon(&mut script),
            3 => run.inbound(&mut script),
            4 => run.set_attachment(&mut script),
            5 => run.closed(&mut script),
            6 => run.outbound(&mut script),
            _ => run.close_request(&mut script),
        }
        run.check_state();
    }
}
