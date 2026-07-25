//! Tantivy search index with classic Libation field names and query normalization.

mod engine;
mod query;

pub use engine::{SearchEngine, SearchHit};
pub use query::normalize_lucene_query;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, SearchError>;

#[derive(Debug, Error)]
pub enum SearchError {
    #[error("search index error: {0}")]
    Index(String),

    #[error("invalid search query: {0}")]
    Query(String),

    #[error("library error: {0}")]
    Library(#[from] bookclerk_library::LibraryError),
}

/// A saved filter expression (Bookclerk quick-filter parity).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SavedFilter {
    pub name: String,
    pub query: String,
}
