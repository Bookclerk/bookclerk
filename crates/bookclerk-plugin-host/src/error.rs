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
