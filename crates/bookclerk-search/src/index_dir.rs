//! Opening, replacing and writing an on-disk Tantivy index directory.
//!
//! The works index ([`crate::WorkSearchIndex`]) opens its directory through
//! these helpers, so an index of an older layout is replaced the same way
//! wherever it is opened.

use std::path::Path;

use tantivy::directory::error::LockError;
use tantivy::schema::Schema;
use tantivy::{Index, TantivyError};

use crate::{Result, SearchError};

/// Serializes replacing an index of an older layout within this process.
pub(crate) static REPLACE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Writer heap of an event refresh: Tantivy's one-thread minimum, since a
/// refresh writes a handful of documents.
pub(crate) const REFRESH_WRITER_HEAP: usize = 15_000_000;

/// First pause a waiting rebuild takes after finding the index busy; each
/// later pause doubles up to [`BUSY_PAUSE_MAX`].
pub(crate) const BUSY_PAUSE_FIRST: std::time::Duration = std::time::Duration::from_millis(100);

/// Longest pause between waiting rebuild attempts.
pub(crate) const BUSY_PAUSE_MAX: std::time::Duration = std::time::Duration::from_secs(2);

/// Maps a Tantivy error to [`SearchError::Busy`] when another writer holds
/// the index lock, else to [`SearchError::Index`].
pub(crate) fn writer_error(err: TantivyError) -> SearchError {
    match err {
        TantivyError::LockFailure(LockError::LockBusy, _) => SearchError::Busy,
        other => SearchError::Index(other.to_string()),
    }
}

/// Opens or creates the Tantivy index in `dir` under `schema`.
///
/// # Errors
///
/// Returns the Tantivy error when the directory cannot be opened, or
/// [`TantivyError::SchemaError`] when it holds an index of another schema.
pub(crate) fn open_index(dir: &Path, schema: Schema) -> tantivy::Result<Index> {
    let directory = tantivy::directory::MmapDirectory::open(dir)
        .map_err(|err| TantivyError::SystemError(err.to_string()))?;
    Index::open_or_create(directory, schema)
}

/// Removes an index of another layout from `dir`.
///
/// Only a directory that holds a Tantivy `meta.json` is touched, so a
/// misconfigured path that names something else is refused rather than
/// emptied.
///
/// # Errors
///
/// Returns [`SearchError::Index`] when `dir` is not a Tantivy index or
/// cannot be removed and recreated.
pub(crate) fn discard_index(dir: &Path) -> Result<()> {
    if !dir.join("meta.json").is_file() {
        return Err(SearchError::Index(format!(
            "{} holds no search index to replace",
            dir.display()
        )));
    }
    tracing::info!(dir = %dir.display(), "replacing a search index of an older layout");
    std::fs::remove_dir_all(dir).map_err(|err| SearchError::Index(err.to_string()))?;
    std::fs::create_dir_all(dir).map_err(|err| SearchError::Index(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_that_is_not_an_index_is_never_emptied() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.txt"), b"mine").unwrap();
        assert!(matches!(
            discard_index(dir.path()),
            Err(SearchError::Index(_))
        ));
        assert!(dir.path().join("keep.txt").is_file());
    }
}
