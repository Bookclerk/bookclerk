use thiserror::Error;

/// Result alias for catalog lookups.
pub type Result<T> = std::result::Result<T, CatalogError>;

/// Failures talking to the public Audible catalog.
#[derive(Debug, Error)]
pub enum CatalogError {
    /// The HTTP client could not be built, the request failed, or the
    /// response did not decode; the message says which.
    #[error("Audible catalog error: {0}")]
    Http(String),
}
