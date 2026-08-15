//! Guest SDK transport and helper errors.
//!
//! Audience: plugin authors calling SDK helpers ([`crate::fetch_work_dir`],
//! [`crate::fetch_work_dir`], authoring tools behind feature `tools`). These
//! errors wrap stdio / framing / I/O failures — not storefront business logic,
//! which should return [`bookclerk_plugin_abi::PluginError`] on the wire.

use thiserror::Error;

/// Result alias for guest helpers that fail with [`SdkError`].
pub type Result<T> = std::result::Result<T, SdkError>;

/// Errors from the guest stdio loop and SDK helpers (not plugin business logic).
///
/// Prefer mapping operator-visible failures into
/// [`bookclerk_plugin_abi::PluginError`] inside [`crate::PluginRoot`]
/// methods. Use [`SdkError`] for framing, JSON, and filesystem problems in the
/// SDK itself.
#[derive(Debug, Error)]
pub enum SdkError {
    /// Operator-facing or diagnostic text with no structured ABI error code.
    ///
    /// Built via [`SdkError::message`]. Typical sources: oversize RPC frames,
    /// missing side-channel descriptors, or authoring-tool validation failures.
    #[error("{0}")]
    Message(String),
    /// Underlying OS I/O failure (stdio, files, sockets) while running SDK helpers.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// JSON serialize/deserialize failure for Workers RPC request or response lines.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

impl SdkError {
    /// Builds a [`SdkError::Message`] from any displayable string.
    ///
    /// # Arguments
    ///
    /// * `msg` - Human-readable explanation (no secret values).
    ///
    /// # Returns
    ///
    /// An [`SdkError`] suitable for helper APIs returning [`Result`].
    pub fn message(msg: impl Into<String>) -> Self {
        Self::Message(msg.into())
    }
}
