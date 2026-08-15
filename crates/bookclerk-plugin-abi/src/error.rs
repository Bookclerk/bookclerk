//! Plugin error model shared by host and guests.
//!
//! Failures on Workers RPC methods serialize as [`PluginError`] inside
//! [`crate::types::RpcResponse::error`] (stdio) or the equivalent workerd
//! reject payload. Codes are stable `snake_case` strings matching
//! `schema/abi.json` `$defs.PluginError.code`. Unknown future codes are
//! preserved as [`PluginErrorCode::Unknown`] plus the raw wire string — they
//! are never collapsed to [`PluginErrorCode::Internal`].

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// Result alias for ABI operations that fail as [`PluginError`].
pub type Result<T> = std::result::Result<T, PluginError>;

/// Stable error codes for plugin RPC failures (schema `PluginError.code`).
///
/// Serialized with `snake_case` wire names (`invalid_params`, `not_found`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginErrorCode {
    /// Request params failed validation or are missing required fields.
    InvalidParams,
    /// Caller is not authenticated for this method (credentials / token).
    Unauthorized,
    /// Caller is authenticated but not allowed to perform this operation.
    Forbidden,
    /// Requested account, object key, session, or row does not exist.
    NotFound,
    /// Backend or dependency is temporarily unreachable (store API, DB, …).
    Unavailable,
    /// Method or capability is not implemented by this guest.
    Unsupported,
    /// Unexpected guest or host failure; see [`PluginError::message`].
    Internal,
    /// A scalar RPC value exceeded [`crate::v2::MAX_SCALAR_BYTES`].
    PayloadTooLarge,
    /// The invocation deadline elapsed before the call completed.
    DeadlineExceeded,
    /// List cursor is missing, stale, or not from this backend.
    InvalidCursor,
    /// The invocation was cancelled (host fence / guest abort).
    Cancelled,
    /// The operation conflicts with current state (conditional put, …).
    Conflict,
    /// Unrecognized wire code. See [`PluginError::wire_str`].
    Unknown,
}

impl PluginErrorCode {
    /// Returns the canonical wire / schema string for this known code.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidParams => "invalid_params",
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::Unavailable => "unavailable",
            Self::Unsupported => "unsupported",
            Self::Internal => "internal",
            Self::PayloadTooLarge => "payload_too_large",
            Self::DeadlineExceeded => "deadline_exceeded",
            Self::InvalidCursor => "invalid_cursor",
            Self::Cancelled => "cancelled",
            Self::Conflict => "conflict",
            Self::Unknown => "unknown",
        }
    }

    /// Maps a wire `code` string onto a known variant, or [`Self::Unknown`].
    #[must_use]
    pub fn from_wire(code: &str) -> Self {
        match code {
            "invalid_params" | "invalidParams" => Self::InvalidParams,
            "unauthorized" | "unauthenticated" => Self::Unauthorized,
            "forbidden" => Self::Forbidden,
            "not_found" | "notFound" => Self::NotFound,
            "unavailable" => Self::Unavailable,
            "unsupported" => Self::Unsupported,
            "internal" => Self::Internal,
            "payload_too_large" | "payloadTooLarge" => Self::PayloadTooLarge,
            "deadline_exceeded" | "deadlineExceeded" => Self::DeadlineExceeded,
            "invalid_cursor" | "invalidCursor" => Self::InvalidCursor,
            "cancelled" | "canceled" => Self::Cancelled,
            "conflict" => Self::Conflict,
            _ => Self::Unknown,
        }
    }
}

impl std::fmt::Display for PluginErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for PluginErrorCode {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PluginErrorCode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Self::from_wire(&s))
    }
}

/// RPC / plugin failure payload returned to the host.
///
/// Wire shape: `{ "code": "…", "message": "…", "details"?: {…} }`. Display
/// formats as `{code}: {message}`. Unknown codes keep the raw wire string in
/// [`Self::wire_code`] / [`Self::wire_str`].
#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[error("{code}: {message}")]
pub struct PluginError {
    /// Machine-stable failure category (wire `code`).
    pub code: PluginErrorCode,
    /// Operator-facing explanation; must not embed secrets.
    pub message: String,
    /// Optional structured extras (validation paths, store status codes, …).
    /// Omitted from JSON when `None`.
    pub details: Option<serde_json::Map<String, serde_json::Value>>,
    /// Raw wire code when [`Self::code`] is [`PluginErrorCode::Unknown`].
    pub wire_code: Option<String>,
}

impl PluginError {
    /// Builds an error with the given code and message and no `details`.
    ///
    /// # Arguments
    ///
    /// * `code` - Stable [`PluginErrorCode`] for the wire `code` field.
    /// * `message` - Human-readable explanation shown to operators.
    #[must_use]
    pub fn new(code: PluginErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
            wire_code: None,
        }
    }

    /// Reconstruct from a wire `code` string without collapsing unknown codes.
    #[must_use]
    pub fn from_wire(code: &str, message: impl Into<String>) -> Self {
        let parsed = PluginErrorCode::from_wire(code);
        Self {
            code: parsed,
            message: message.into(),
            details: None,
            wire_code: if parsed == PluginErrorCode::Unknown {
                Some(code.to_string())
            } else {
                None
            },
        }
    }

    /// Wire `code` string (raw unknown code when present).
    #[must_use]
    pub fn wire_str(&self) -> &str {
        if self.code == PluginErrorCode::Unknown {
            self.wire_code.as_deref().unwrap_or("unknown")
        } else {
            self.code.as_str()
        }
    }

    /// Convenience for [`PluginErrorCode::InvalidParams`].
    #[must_use]
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(PluginErrorCode::InvalidParams, message)
    }

    /// Convenience for [`PluginErrorCode::Unsupported`].
    #[must_use]
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(PluginErrorCode::Unsupported, message)
    }

    /// Convenience for [`PluginErrorCode::Internal`].
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(PluginErrorCode::Internal, message)
    }

    /// Convenience for [`PluginErrorCode::NotFound`].
    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(PluginErrorCode::NotFound, message)
    }

    /// Convenience for [`PluginErrorCode::Unavailable`].
    ///
    /// Use for ambiguous transport failures (lost HTTP/RPC responses, timeouts)
    /// where the caller should retry the same idempotency key.
    #[must_use]
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(PluginErrorCode::Unavailable, message)
    }

    /// Convenience for [`PluginErrorCode::PayloadTooLarge`].
    #[must_use]
    pub fn payload_too_large(message: impl Into<String>) -> Self {
        Self::new(PluginErrorCode::PayloadTooLarge, message)
    }

    /// Convenience for [`PluginErrorCode::DeadlineExceeded`].
    #[must_use]
    pub fn deadline_exceeded(message: impl Into<String>) -> Self {
        Self::new(PluginErrorCode::DeadlineExceeded, message)
    }

    /// Convenience for [`PluginErrorCode::Forbidden`].
    #[must_use]
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(PluginErrorCode::Forbidden, message)
    }

    /// Convenience for [`PluginErrorCode::InvalidCursor`].
    #[must_use]
    pub fn invalid_cursor(message: impl Into<String>) -> Self {
        Self::new(PluginErrorCode::InvalidCursor, message)
    }

    /// Convenience for [`PluginErrorCode::Cancelled`].
    #[must_use]
    pub fn cancelled(message: impl Into<String>) -> Self {
        Self::new(PluginErrorCode::Cancelled, message)
    }

    /// Convenience for [`PluginErrorCode::Conflict`].
    #[must_use]
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(PluginErrorCode::Conflict, message)
    }
}

impl Serialize for PluginError {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut n = 2;
        if self.details.is_some() {
            n += 1;
        }
        let mut state = serializer.serialize_struct("PluginError", n)?;
        state.serialize_field("code", self.wire_str())?;
        state.serialize_field("message", &self.message)?;
        if let Some(details) = &self.details {
            state.serialize_field("details", details)?;
        }
        state.end()
    }
}

impl<'de> Deserialize<'de> for PluginError {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            code: String,
            message: String,
            #[serde(default)]
            details: Option<serde_json::Map<String, serde_json::Value>>,
        }
        let raw = Raw::deserialize(deserializer)?;
        let mut err = PluginError::from_wire(&raw.code, raw.message);
        err.details = raw.details;
        Ok(err)
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    #[test]
    fn unknown_wire_code_is_preserved() {
        let err = PluginError::from_wire("future_retry_policy", "try later");
        assert_eq!(err.code, PluginErrorCode::Unknown);
        assert_eq!(err.wire_str(), "future_retry_policy");
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["code"], "future_retry_policy");
        let back: PluginError = serde_json::from_value(v).unwrap();
        assert_eq!(back.code, PluginErrorCode::Unknown);
        assert_eq!(back.wire_str(), "future_retry_policy");
    }

    #[test]
    fn known_code_is_not_internal_when_message_mentions_not_found() {
        let err = PluginError::from_wire("internal", "object not_found in cache");
        assert_eq!(err.code, PluginErrorCode::Internal);
        assert_eq!(err.message, "object not_found in cache");
    }
}
