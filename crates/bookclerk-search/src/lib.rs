//! Tantivy search index with classic Libation field names and query normalization.
//!
//! # Audience
//!
//! Host library / GUI code that builds and queries the on-disk search index.
//! Guest plugins do not depend on this crate.
//!
//! Style: `docs/code-documentation.md`.

mod engine;
mod query;

pub use engine::{SearchEngine, SearchHit};
pub use query::normalize_lucene_query;

use thiserror::Error;

/// Result alias for search index and query helpers in this crate.
pub type Result<T> = std::result::Result<T, SearchError>;

/// Errors from opening, writing, or querying the Tantivy index.
#[derive(Debug, Error)]
pub enum SearchError {
    /// Index open / writer / commit failure (operator-facing detail).
    #[error("search index error: {0}")]
    Index(String),

    /// Query parse failure after Lucene-style normalization.
    #[error("invalid search query: {0}")]
    Query(String),

    /// Failure loading library rows while rebuilding the index.
    #[error("library error: {0}")]
    Library(#[from] bookclerk_library::LibraryError),
}

/// A saved filter expression (Bookclerk quick-filter parity).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SavedFilter {
    /// Operator-facing display name for this saved filter.
    pub name: String,
    /// Lucene-style query string (normalized before parse).
    pub query: String,
}
