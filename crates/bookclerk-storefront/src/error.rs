//! Error types for content-source auth, scan, and fetch.
//!
//! # Audience
//!
//! Storefront guests and host job runners.

use thiserror::Error;

/// Result alias for content-source operations.
pub type Result<T> = std::result::Result<T, SourceError>;

/// Errors from content-source auth, scan, or fetch.
#[derive(Debug, Error)]
pub enum SourceError {
    /// No accounts are configured for this source (safe to skip in multi-source scan).
    #[error("no accounts configured for source: {0}")]
    NoAccounts(String),
    /// Authentication or credential failure (operator-facing message).
    #[error("{0}")]
    Auth(String),
    /// Upstream store HTTP / protocol failure (operator-facing message).
    #[error("{0}")]
    Api(String),
    /// The store asked to be left alone for a while (HTTP 429 or its
    /// equivalent); the duration is its `Retry-After`, when it sent one.
    #[error("rate limited by the store")]
    RateLimited(Option<std::time::Duration>),
    /// Local filesystem failure while caching or writing downloads.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// Library database / secret-store failure on the host.
    ///
    /// Guests never open the library; the host converts its store errors
    /// into this variant (`bookclerk-library` implements the `From`).
    #[error("{0}")]
    Library(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
    /// Catch-all for plugin-internal failures wrapped as [`anyhow::Error`].
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

impl From<SourceError> for bookclerk_plugin_abi::PluginError {
    /// Maps a storefront failure onto the typed ABI error a guest returns:
    /// a rate limit keeps its wait, an authentication failure its code, and
    /// the rest are `internal` with the message.
    fn from(err: SourceError) -> Self {
        match err {
            SourceError::RateLimited(wait) => Self::rate_limited(err.to_string(), wait),
            SourceError::Auth(message) => {
                Self::new(bookclerk_plugin_abi::PluginErrorCode::Unauthorized, message)
            }
            SourceError::NoAccounts(_)
            | SourceError::Api(_)
            | SourceError::Io(_)
            | SourceError::Library(_)
            | SourceError::Other(_) => Self::internal(err.to_string()),
        }
    }
}

impl SourceError {
    /// [`Self::RateLimited`] from an HTTP status and `Retry-After` header,
    /// when the status is 429; otherwise `None`.
    ///
    /// # Arguments
    ///
    /// * `status` - The response status code.
    /// * `retry_after` - The `Retry-After` header value: whole seconds, as
    ///   stores send it (an HTTP date is ignored).
    #[must_use]
    pub fn from_rate_limit(status: u16, retry_after: Option<&str>) -> Option<Self> {
        if status != 429 {
            return None;
        }
        let wait = retry_after
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(std::time::Duration::from_secs);
        Some(Self::RateLimited(wait))
    }

    /// Construct [`Self::NoAccounts`] with an operator-facing message.
    #[must_use]
    pub fn no_accounts(msg: impl Into<String>) -> Self {
        Self::NoAccounts(msg.into())
    }

    /// Construct [`Self::Auth`] with an operator-facing message.
    #[must_use]
    pub fn auth(msg: impl Into<String>) -> Self {
        Self::Auth(msg.into())
    }

    /// Construct [`Self::Api`] with an operator-facing message.
    #[must_use]
    pub fn api(msg: impl Into<String>) -> Self {
        Self::Api(msg.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_429_becomes_a_rate_limit_with_the_wait_the_store_sent() {
        assert!(SourceError::from_rate_limit(200, None).is_none());
        assert!(SourceError::from_rate_limit(503, Some("5")).is_none());
        assert!(matches!(
            SourceError::from_rate_limit(429, None),
            Some(SourceError::RateLimited(None))
        ));
        assert!(matches!(
            SourceError::from_rate_limit(429, Some(" 30 ")),
            Some(SourceError::RateLimited(Some(wait))) if wait.as_secs() == 30
        ));
        // An HTTP-date Retry-After is not parsed; the host picks its default.
        assert!(matches!(
            SourceError::from_rate_limit(429, Some("Wed, 21 Oct 2026 07:28:00 GMT")),
            Some(SourceError::RateLimited(None))
        ));
        let err: bookclerk_plugin_abi::PluginError =
            SourceError::RateLimited(Some(std::time::Duration::from_secs(7))).into();
        assert_eq!(err.code, bookclerk_plugin_abi::PluginErrorCode::RateLimited);
        assert_eq!(err.retry_after(), Some(std::time::Duration::from_secs(7)));
        let err: bookclerk_plugin_abi::PluginError = SourceError::api("down").into();
        assert_eq!(err.code, bookclerk_plugin_abi::PluginErrorCode::Internal);
    }
}
