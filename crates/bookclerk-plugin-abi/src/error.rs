//! Plugin error model shared by host and guests.
//!
//! Failures on Workers RPC methods serialize as [`PluginError`] inside
//! [`crate::types::RpcResponse::error`] (stdio) or the equivalent workerd
//! reject payload. Codes are stable `snake_case` strings matching
//! `schema/abi.json` `$defs.PluginError.code`.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Result alias for ABI operations that fail as [`PluginError`].
pub type Result<T> = std::result::Result<T, PluginError>;

/// Stable error codes for plugin RPC failures (schema `PluginError.code`).
///
/// Serialized with `snake_case` wire names (`invalid_params`, `not_found`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginErrorCode {
    /// Request params failed validation or are missing required fields.
    InvalidParams,
    /// Caller is not authenticated for this method (credentials / token).
    Unauthorized,
    /// Caller is authenticated but not allowed to perform the operation.
    Forbidden,
    /// Requested account, object key, session, or row does not exist.
    NotFound,
    /// Backend or dependency is temporarily unreachable (store API, DB, …).
    Unavailable,
    /// Method or capability is not implemented by this guest.
    Unsupported,
    /// Unexpected guest or host failure; see [`PluginError::message`].
    Internal,
}

impl PluginErrorCode {
    /// Returns the canonical wire / schema string for this code.
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
        }
    }
}

impl std::fmt::Display for PluginErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// RPC / plugin failure payload returned to the host.
///
/// Wire shape: `{ "code": "…", "message": "…", "details"?: {…} }`. Display
/// formats as `{code}: {message}`.
#[derive(Debug, Clone, Error, Serialize, Deserialize, PartialEq, Eq)]
#[error("{code}: {message}")]
pub struct PluginError {
    /// Machine-stable failure category (wire `code`).
    pub code: PluginErrorCode,
    /// Operator-facing explanation; must not embed secrets.
    pub message: String,
    /// Optional structured extras (validation paths, store status codes, …).
    /// Omitted from JSON when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Map<String, serde_json::Value>>,
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
}
