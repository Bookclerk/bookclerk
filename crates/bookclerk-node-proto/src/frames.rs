//! JSON frames `{v, id, type, ...}`.

use serde::{Deserialize, Serialize};

use crate::{FRAME_VERSION, MAX_FRAME_BYTES};

/// A lease the host still holds when it reconnects (`hello.resume`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeldFence {
    /// `job` or `delivery`.
    pub kind: String,
    /// Job id or delivery id.
    pub id: String,
    /// Lease generation the host holds.
    pub fence: i64,
}

/// What a frame says, tagged by `type`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Body {
    /// Host → owner: first frame.
    Hello {
        /// Node id the owner minted at enrollment.
        node_id: String,
        /// Host process incarnation (new on each start).
        incarnation: String,
        /// Host's Bookclerk version.
        #[serde(default)]
        software_version: String,
        /// Leases the host still holds from an earlier connection.
        #[serde(default)]
        resume: Vec<HeldFence>,
    },
    /// Owner → host: the nonce to sign.
    Challenge {
        /// 32 random bytes, base64url without padding.
        nonce: String,
        /// Owner cluster id (also signed).
        cluster_id: String,
    },
    /// Host → owner: proof of key possession.
    Auth {
        /// Ed25519 signature over [`crate::auth_transcript`], base64url
        /// without padding.
        sig: String,
    },
    /// Owner → host: the handshake succeeded.
    Welcome {
        /// Authenticated node id.
        node_id: String,
        /// Owner cluster id.
        cluster_id: String,
    },
    /// Either side: a request was not served. The connection stays open.
    Error {
        /// Stable code (`unsupported`, `invalid`, ...).
        code: String,
        /// Human-readable detail.
        #[serde(default)]
        message: String,
    },
    /// A frame type this side does not know.
    #[serde(other)]
    Unknown,
}

/// One frame: the envelope and its body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frame {
    /// Envelope version; always [`FRAME_VERSION`].
    pub v: u32,
    /// Request id, echoed by the answer.
    pub id: String,
    /// The body, tagged by `type`.
    #[serde(flatten)]
    pub body: Body,
}

/// Why a frame was not accepted.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FrameError {
    /// More than [`MAX_FRAME_BYTES`].
    #[error("frame exceeds {MAX_FRAME_BYTES} bytes")]
    TooLarge,
    /// Not a JSON frame of the expected shape.
    #[error("malformed frame")]
    Malformed,
    /// `v` is not [`FRAME_VERSION`].
    #[error("unsupported frame version {0}")]
    Version(u32),
}

impl Frame {
    /// A version-1 frame.
    #[must_use]
    pub fn new(id: impl Into<String>, body: Body) -> Self {
        Self {
            v: FRAME_VERSION,
            id: id.into(),
            body,
        }
    }

    /// Parses a text frame, checking size and version.
    ///
    /// # Errors
    ///
    /// Returns [`FrameError`] for an oversized, malformed, or wrong-version
    /// frame.
    pub fn parse(text: &str) -> Result<Self, FrameError> {
        if text.len() > MAX_FRAME_BYTES {
            return Err(FrameError::TooLarge);
        }
        let frame: Self = serde_json::from_str(text).map_err(|_| FrameError::Malformed)?;
        if frame.v != FRAME_VERSION {
            return Err(FrameError::Version(frame.v));
        }
        Ok(frame)
    }

    /// Serializes the frame as JSON text.
    #[must_use]
    pub fn to_text(&self) -> String {
        // A frame is plain strings and numbers; serializing cannot fail.
        serde_json::to_string(self).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_unknown_types_parse() {
        let hello = Frame::new(
            "1",
            Body::Hello {
                node_id: "n".into(),
                incarnation: "i".into(),
                software_version: "1.0.0".into(),
                resume: vec![HeldFence {
                    kind: "job".into(),
                    id: "7".into(),
                    fence: 3,
                }],
            },
        );
        let text = hello.to_text();
        assert!(text.contains(r#""type":"hello""#), "{text}");
        assert_eq!(Frame::parse(&text).unwrap(), hello);

        let unknown = Frame::parse(r#"{"v":1,"id":"2","type":"claim","item":"job"}"#).unwrap();
        assert_eq!(unknown.body, Body::Unknown);
        assert_eq!(
            Frame::parse(r#"{"v":2,"id":"2","type":"auth","sig":"x"}"#),
            Err(FrameError::Version(2))
        );
        assert_eq!(Frame::parse("[]"), Err(FrameError::Malformed));
        let huge = format!(
            r#"{{"v":1,"id":"{}","type":"auth","sig":""}}"#,
            "x".repeat(MAX_FRAME_BYTES)
        );
        assert_eq!(Frame::parse(&huge), Err(FrameError::TooLarge));
    }
}
