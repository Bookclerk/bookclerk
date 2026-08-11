use thiserror::Error;

/// Result alias for discovery operations.
pub type Result<T> = std::result::Result<T, DiscoverError>;

/// Errors from embeddings, catalog fan-out, and recommendation pipelines.
#[derive(Debug, Error)]
pub enum DiscoverError {
    /// Error propagated from [`bookclerk_library`].
    #[error(transparent)]
    Library(#[from] bookclerk_library::LibraryError),
    /// Error propagated from [`bookclerk_enrich`].
    #[error(transparent)]
    Enrich(#[from] bookclerk_enrich::EnrichError),
    /// Outbound HTTP failure talking to a storefront or Open Library.
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    /// Embedding model load / inference failure.
    #[error("embedding: {0}")]
    Embed(String),
    /// Operator-facing error text with no structured code.
    #[error("{0}")]
    Message(String),
}

impl DiscoverError {
    /// Builds a [`DiscoverError::Message`] from operator-facing text.
    ///
    /// # Arguments
    ///
    /// * `msg` - Human-readable error detail (no secrets).
    ///
    /// # Returns
    ///
    /// A [`DiscoverError::Message`] variant.
    #[must_use]
    pub fn message(msg: impl Into<String>) -> Self {
        Self::Message(msg.into())
    }
}
