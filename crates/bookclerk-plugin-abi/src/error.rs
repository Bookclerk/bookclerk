//! Plugin error model shared by host and guests.
//!
//! Capability failures serialize as [`PluginError`] in the Cap'n Proto
//! `PluginError` struct (or the equivalent workerd reject payload). Codes are
//! stable `snake_case` strings matching the `PluginErrorCode` enum in
//! `schema/plugin.capnp`. Unknown future codes are preserved as
//! [`PluginErrorCode::Unknown`] plus the raw wire string — they are never
//! collapsed to [`PluginErrorCode::Internal`].

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
    /// A scalar RPC value exceeded [`crate::MAX_SCALAR_BYTES`].
    PayloadTooLarge,
    /// The invocation deadline elapsed before the call completed.
    DeadlineExceeded,
    /// List cursor is missing, stale, or not from this backend.
    InvalidCursor,
    /// The invocation was cancelled (host fence / guest abort).
    Cancelled,
    /// The operation conflicts with current state (conditional put, …).
    Conflict,
    /// A negotiated database limit was exceeded; [`PluginError::limit`]
    /// names which one.
    LimitExceeded,
    /// The backend asked the guest to slow down (HTTP 429 or its
    /// equivalent); [`PluginError::retry_after`] says for how long.
    RateLimited,
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
            Self::LimitExceeded => "limit_exceeded",
            Self::RateLimited => "rate_limited",
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
            "conflict" | "unique" | "constraint" | "unique_constraint" => Self::Conflict,
            "limit_exceeded" | "limitExceeded" => Self::LimitExceeded,
            "rate_limited" | "rateLimited" | "too_many_requests" => Self::RateLimited,
            "timeout" | "timed_out" => Self::DeadlineExceeded,
            "retryable" => Self::Unavailable,
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

    /// Convenience for [`PluginErrorCode::RateLimited`].
    ///
    /// # Arguments
    ///
    /// * `message` - Human-readable explanation (which backend, which call).
    /// * `retry_after` - How long the backend asked to be left alone, when
    ///   it said (its `Retry-After`); stored as `details.retryAfterMs`.
    #[must_use]
    pub fn rate_limited(
        message: impl Into<String>,
        retry_after: Option<std::time::Duration>,
    ) -> Self {
        let mut err = Self::new(PluginErrorCode::RateLimited, message);
        if let Some(wait) = retry_after {
            let ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX);
            err.details = Some(
                std::iter::once((String::from("retryAfterMs"), serde_json::Value::from(ms)))
                    .collect(),
            );
        }
        err
    }

    /// How long a [`PluginErrorCode::RateLimited`] error asks the caller to
    /// wait, when the backend said (`details.retryAfterMs`).
    #[must_use]
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        if self.code != PluginErrorCode::RateLimited {
            return None;
        }
        self.details
            .as_ref()
            .and_then(|d| d.get("retryAfterMs"))
            .and_then(serde_json::Value::as_u64)
            .map(std::time::Duration::from_millis)
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

    /// Builds a [`PluginErrorCode::LimitExceeded`] error whose `details`
    /// carry `limit`, `cap` and `used` and whose message is
    /// [`LimitExceeded::to_message`].
    #[must_use]
    pub fn limit_exceeded(limit: LimitExceeded) -> Self {
        let mut out = Self::new(PluginErrorCode::LimitExceeded, limit.to_message());
        out.details = Some(limit.details());
        out
    }

    /// The exceeded limit when this is a [`PluginErrorCode::LimitExceeded`]
    /// error.
    ///
    /// Reads the structured `details` first and falls back to the message
    /// text, so an error relayed by a hop that dropped `details` still
    /// names its limit.
    #[must_use]
    pub fn limit(&self) -> Option<LimitExceeded> {
        if self.code != PluginErrorCode::LimitExceeded {
            return None;
        }
        self.details
            .as_ref()
            .and_then(LimitExceeded::from_details)
            .or_else(|| LimitExceeded::from_message(&self.message))
    }
}

/// One exceeded negotiated limit: which cap, its value, and the value that
/// exceeded it.
///
/// Every emitter (adapter SDK, host reply checks, SDK request checks) formats
/// the same message through [`Self::to_message`], and every classifier parses
/// it back through [`Self::from_message`], so a limit error stays typed across
/// a `DbErr` string hop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitExceeded {
    /// Capability field name as advertised (`maxResultBytes`, `maxResultRows`,
    /// `maxRequestBytes`, ...).
    pub limit: String,
    /// The negotiated cap.
    pub cap: u64,
    /// The observed value that exceeded `cap`.
    pub used: u64,
}

/// Message prefix every limit error starts with.
const LIMIT_MESSAGE_PREFIX: &str = "limit exceeded: ";

impl LimitExceeded {
    /// Builds the record.
    #[must_use]
    pub fn new(limit: impl Into<String>, cap: u64, used: u64) -> Self {
        Self {
            limit: limit.into(),
            cap,
            used,
        }
    }

    /// Canonical message: `limit exceeded: maxResultBytes is 262144; used 300000`.
    #[must_use]
    pub fn to_message(&self) -> String {
        format!(
            "{LIMIT_MESSAGE_PREFIX}{} is {}; used {}",
            self.limit, self.cap, self.used
        )
    }

    /// Parses a message produced by [`Self::to_message`], possibly followed by
    /// further text after a `;` or `:`.
    #[must_use]
    pub fn from_message(message: &str) -> Option<Self> {
        let start = message.find(LIMIT_MESSAGE_PREFIX)?;
        let rest = &message[start + LIMIT_MESSAGE_PREFIX.len()..];
        let (limit, rest) = rest.split_once(" is ")?;
        let (cap, rest) = rest.split_once("; used ")?;
        let used_end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if !is_limit_name(limit) {
            return None;
        }
        Some(Self {
            limit: limit.to_string(),
            cap: cap.parse().ok()?,
            used: rest[..used_end].parse().ok()?,
        })
    }

    /// The `details` object: `{ "limit": ..., "cap": ..., "used": ... }`.
    #[must_use]
    pub fn details(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut map = serde_json::Map::new();
        map.insert("limit".into(), serde_json::Value::from(self.limit.clone()));
        map.insert("cap".into(), serde_json::Value::from(self.cap));
        map.insert("used".into(), serde_json::Value::from(self.used));
        map
    }

    /// Reads the record back from a `details` object; `None` when any field
    /// is missing or not of the expected type, or when `limit` is not a
    /// name [`Self::from_message`] would accept, so a record read from
    /// `details` always round-trips through its canonical message.
    #[must_use]
    pub fn from_details(details: &serde_json::Map<String, serde_json::Value>) -> Option<Self> {
        let limit = details.get("limit")?.as_str()?;
        if !is_limit_name(limit) {
            return None;
        }
        Some(Self {
            limit: limit.to_string(),
            cap: details.get("cap")?.as_u64()?,
            used: details.get("used")?.as_u64()?,
        })
    }
}

/// True when `name` is a cap name both limit parsers accept: non-empty ASCII
/// letters, digits and `_` (the SQL-v1 `maxResultBytes` style), so it cannot
/// contain the `" is "` and `"; used "` separators of the canonical message.
fn is_limit_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

impl std::fmt::Display for LimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_message())
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
    fn a_rate_limit_keeps_its_wait_across_the_wire() {
        let err =
            PluginError::rate_limited("slow down", Some(std::time::Duration::from_millis(1500)));
        assert_eq!(err.code, PluginErrorCode::RateLimited);
        assert_eq!(err.wire_str(), "rate_limited");
        assert_eq!(
            err.retry_after(),
            Some(std::time::Duration::from_millis(1500))
        );
        assert_eq!(
            err.details.as_ref().and_then(|d| d.get("retryAfterMs")),
            Some(&serde_json::Value::from(1500u64))
        );
        assert!(PluginError::rate_limited("slow down", None)
            .retry_after()
            .is_none());
        // Another code never reports a wait, even with the detail present.
        let mut other = PluginError::internal("x");
        other.details = err.details.clone();
        assert!(other.retry_after().is_none());
        assert_eq!(
            PluginErrorCode::from_wire("rate_limited"),
            PluginErrorCode::RateLimited
        );
        assert_eq!(
            PluginErrorCode::from_wire("too_many_requests"),
            PluginErrorCode::RateLimited
        );
    }

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
    fn limit_exceeded_round_trips_through_message_and_details() {
        let limit = LimitExceeded::new("maxResultBytes", 262_144, 300_000);
        let err = PluginError::limit_exceeded(limit.clone());
        assert_eq!(err.code, PluginErrorCode::LimitExceeded);
        assert_eq!(
            err.message,
            "limit exceeded: maxResultBytes is 262144; used 300000"
        );
        assert_eq!(err.limit(), Some(limit.clone()));
        assert_eq!(
            LimitExceeded::from_message(&err.message),
            Some(limit.clone())
        );
        assert_eq!(
            LimitExceeded::from_message("execute reply statement 2: limit exceeded: maxCellBytes is 1024; used 4096 (column `body`)"),
            Some(LimitExceeded::new("maxCellBytes", 1024, 4096))
        );
        assert_eq!(
            LimitExceeded::from_message("limit exceeded: is 1; used 2"),
            None
        );
        assert_eq!(
            LimitExceeded::from_message("limit exceeded: maxBinds is x; used 2"),
            None
        );
        let mut relayed = PluginError::from_wire("limit_exceeded", err.message.clone());
        assert_eq!(relayed.limit(), Some(limit.clone()));
        relayed.details = Some(serde_json::Map::new());
        assert_eq!(relayed.limit(), Some(limit));
        assert_eq!(
            PluginError::conflict("limit exceeded: maxBinds is 1; used 2").limit(),
            None
        );
    }

    #[test]
    fn details_with_a_name_the_message_grammar_rejects_fall_back_to_the_message() {
        // Both parsers accept the same names, so a record read from
        // `details` always round-trips through its canonical message.
        let mut details = serde_json::Map::new();
        details.insert("limit".into(), serde_json::Value::from("maxResult-Bytes"));
        details.insert("cap".into(), serde_json::Value::from(1u64));
        details.insert("used".into(), serde_json::Value::from(2u64));
        assert_eq!(LimitExceeded::from_details(&details), None);
        let mut err = PluginError::from_wire(
            "limit_exceeded",
            LimitExceeded::new("maxResultBytes", 1, 2).to_message(),
        );
        err.details = Some(details.clone());
        assert_eq!(
            err.limit(),
            Some(LimitExceeded::new("maxResultBytes", 1, 2))
        );

        details.insert("limit".into(), serde_json::Value::from(""));
        assert_eq!(LimitExceeded::from_details(&details), None);
        details.insert("limit".into(), serde_json::Value::from("max_cell_bytes2"));
        let parsed = LimitExceeded::from_details(&details).expect("valid name");
        assert_eq!(
            LimitExceeded::from_message(&parsed.to_message()),
            Some(parsed)
        );
    }

    #[test]
    fn known_code_is_not_internal_when_message_mentions_not_found() {
        let err = PluginError::from_wire("internal", "object not_found in cache");
        assert_eq!(err.code, PluginErrorCode::Internal);
        assert_eq!(err.message, "object not_found in cache");
    }
}
