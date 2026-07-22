use thiserror::Error;

pub type Result<T> = std::result::Result<T, EnrichError>;

#[derive(Debug, Error)]
pub enum EnrichError {
    #[error("enrichment error: {0}")]
    Sync(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Library(#[from] libation_library::LibraryError),
}
