//! Plugin host/guest errors.

use thiserror::Error;

/// Result alias for [`PluginError`].
pub type Result<T> = std::result::Result<T, PluginError>;

/// Failures from discovery, RPC, consent, or plugin adapters.
#[derive(Debug, Error)]
pub enum PluginError {
    /// Operator-facing error text with no structured code.
    #[error("{0}")]
    Message(String),
    /// Backend or plugin transport is temporarily unreachable; retry the same
    /// idempotency key when the caller still holds consume-once material.
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// Filesystem or process I/O failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// JSON encode/decode failure on the RPC wire or grant file.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// TOML decode failure (settings tables, legacy paths).
    #[error("toml: {0}")]
    Toml(#[from] toml::de::Error),
    /// Invalid or unsupported `plugin.toml` / package manifest.
    #[error(transparent)]
    Manifest(#[from] bookclerk_plugin_manifest::Error),
    /// Catch-all for otherwise unclassified failures.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl PluginError {
    /// Builds a [`PluginError::Message`] from any displayable string.
    ///
    /// # Arguments
    ///
    /// * `msg` - Operator-facing explanation; must not embed secrets.
    ///
    /// # Returns
    ///
    /// A message-only [`PluginError`].
    #[must_use]
    pub fn message(msg: impl Into<String>) -> Self {
        Self::Message(msg.into())
    }

    /// Builds a structured unavailable error (lost RPC, timeout, incomplete reply).
    #[must_use]
    pub fn unavailable(msg: impl Into<String>) -> Self {
        Self::Unavailable(msg.into())
    }

    /// Maps a guest ABI error, preserving [`unavailable`](Self::unavailable)
    /// so atomic retries can classify without string matching.
    #[must_use]
    pub fn from_abi(code: Option<&str>, message: impl Into<String>) -> Self {
        let message = message.into();
        if code == Some("unavailable") {
            Self::unavailable(message)
        } else {
            Self::message(message)
        }
    }

    /// True when a `dbAtomic` caller should retry the same operation id.
    ///
    /// Relies on the structured [`Self::Unavailable`] variant (guest ABI
    /// `unavailable`, RPC timeout, or a closed guest) rather than matching
    /// `D1 HTTP` strings: permanent 4xx from D1 is `internal` / `invalid_params`.
    #[must_use]
    pub fn is_ambiguous_transport(&self) -> bool {
        match self {
            Self::Unavailable(_) => true,
            Self::Io(err) => matches!(
                err.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::UnexpectedEof
            ),
            _ => false,
        }
    }
}

impl From<PluginError> for bookclerk_source::SourceError {
    fn from(err: PluginError) -> Self {
        bookclerk_source::SourceError::api(err.to_string())
    }
}

impl From<PluginError> for bookclerk_integrations::IntegrationError {
    fn from(err: PluginError) -> Self {
        bookclerk_integrations::IntegrationError::message(err.to_string())
    }
}
