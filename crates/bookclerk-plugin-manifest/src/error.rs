//! Manifest parse and semantic-validation errors.
//!
//! All fallible entry points in this crate ([`crate::parse`],
//! [`crate::validate_plugin_id`], [`crate::validate_logo`],
//! [`crate::format_manifest`], and so on) return [`Result`] with this
//! [`enum@Error`] type. TOML serde failures are preserved via
//! [`Error::TomlDe`] / [`Error::TomlSer`]; semantic rules use
//! [`Error::Message`].

use thiserror::Error;

/// Result alias for manifest parse, validate, and format operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors produced while deserializing or validating `plugin.toml`.
///
/// Display implementations are operator-facing (prefixed where useful, e.g.
/// `toml: …`). There is no structured error code; callers typically match on
/// the variant or format the message for CLI / doctor output.
#[derive(Debug, Error)]
pub enum Error {
    /// Operator-facing semantic failure with no structured code.
    ///
    /// Used for id grammar, runtime field requirements, network domain rules,
    /// logo validation, and similar checks after TOML deserialize succeeds.
    #[error("{0}")]
    Message(String),

    /// TOML deserialize failure from `toml::from_str`.
    #[error("toml: {0}")]
    TomlDe(#[from] toml::de::Error),

    /// TOML serialize failure from canonical [`crate::format_manifest`].
    #[error("toml serialize: {0}")]
    TomlSer(#[from] toml::ser::Error),
}

impl Error {
    /// Builds an [`Error::Message`] from any string-like value.
    ///
    /// # Arguments
    ///
    /// * `msg` - Human-readable explanation (often prefixed with `plugin.toml:`).
    ///
    /// # Returns
    ///
    /// An [`Error::Message`] wrapping the owned string.
    #[must_use]
    pub fn message(msg: impl Into<String>) -> Self {
        Self::Message(msg.into())
    }
}
