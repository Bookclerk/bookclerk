use thiserror::Error;

/// Result alias for this crate's error type.
pub type Result<T> = std::result::Result<T, EnrichError>;

/// Errors from public-catalog enrichment and match scoring.
#[derive(Debug, Error)]
pub enum EnrichError {
    /// Failure during a blocking / sync enrich step.
    #[error("enrichment error: {0}")]
    Sync(String),

    /// Underlying filesystem or stream I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Error propagated from [`bookclerk_library`].
    #[error(transparent)]
    Library(#[from] bookclerk_library::LibraryError),
}
