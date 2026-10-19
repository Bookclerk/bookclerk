//! Wire protocol between a Bookclerk host node and its owner cluster.
//!
//! A host connects to `GET /api/node/v1/connect` with the WebSocket
//! subprotocol [`SUBPROTOCOL`] and proves itself with the Ed25519 key it
//! enrolled (H-4 design, section 3):
//!
//! 1. host → owner: [`Body::Hello`] `{node_id, incarnation,
//!    software_version, resume}`
//! 2. owner → host: [`Body::Challenge`] `{nonce, cluster_id}`
//! 3. host → owner: [`Body::Auth`] `{sig}`, an Ed25519 signature over
//!    [`auth_transcript`]
//! 4. owner → host: [`Body::Welcome`], or a close with
//!    [`CLOSE_AUTH_FAILED`] / [`CLOSE_REVOKED`].
//!
//! Frames are JSON text of at most [`MAX_FRAME_BYTES`]; binary frames are a
//! protocol error because bulk data travels over HTTP.
//!
//! This crate is pure: no I/O, no clock, no randomness. The daemon (owner)
//! and the host client share it so both sides sign and check the same
//! bytes.

mod frames;
mod keys;
mod signature;

pub use ed25519_dalek::SigningKey;
pub use frames::{Body, Frame, FrameError, HeldFence};
pub use keys::{
    box_public_key, sign_public_key, validate_box_public_key, validate_sign_public_key, KeyError,
    PUBLIC_KEY_LEN,
};
pub use signature::{
    auth_transcript, canonical_origin, decode_nonce, encode_nonce, sign_auth, verify_auth,
    AuthContext, AuthError, AUTH_DOMAIN, NONCE_LEN,
};

/// WebSocket subprotocol a host must offer.
pub const SUBPROTOCOL: &str = "bookclerk-node.v1";

/// Frame envelope version (`v`).
pub const FRAME_VERSION: u32 = 1;

/// Largest frame either side accepts (1 MiB).
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Seconds between pings from each side.
pub const PING_INTERVAL_SECS: u64 = 20;

/// Seconds without any inbound frame before a side closes the connection.
pub const IDLE_TIMEOUT_SECS: u64 = 60;

/// Seconds the owner waits for each handshake frame.
pub const HANDSHAKE_TIMEOUT_SECS: u64 = 10;

/// Close code: the handshake failed (unknown node, bad signature, or a
/// malformed or late handshake frame).
pub const CLOSE_AUTH_FAILED: u16 = 4401;

/// Close code: the node's credential is revoked.
pub const CLOSE_REVOKED: u16 = 4403;

/// Close code: a newer authenticated connection for the same node replaced
/// this one.
pub const CLOSE_SUPERSEDED: u16 = 4409;

/// Close code: a frame broke the protocol after the handshake (binary,
/// oversized, malformed, or the wrong version).
pub const CLOSE_PROTOCOL: u16 = 4400;
