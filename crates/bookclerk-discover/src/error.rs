use thiserror::Error;

pub type Result<T> = std::result::Result<T, DiscoverError>;

#[derive(Debug, Error)]
pub enum DiscoverError {
    #[error(transparent)]
    Library(#[from] bookclerk_library::LibraryError),
    #[error(transparent)]
    Enrich(#[from] bookclerk_enrich::EnrichError),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error("embedding: {0}")]
    Embed(String),
    #[error("{0}")]
    Message(String),
}

impl DiscoverError {
    #[must_use]
    pub fn message(msg: impl Into<String>) -> Self {
        Self::Message(msg.into())
    }
}
