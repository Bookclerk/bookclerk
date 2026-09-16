//! Open a destination [`LibraryStore`] without linking adapter implementation crates.

use std::path::Path;

use bookclerk_library::LibraryStore;

use crate::error::Result;

/// Destination library plus any throwaway directory that must outlive it.
pub(crate) struct DestStore {
    /// Open library used by import/export.
    pub store: LibraryStore,
    /// Holds a dry-run temp files dir so the adapter's sqlite file stays alive.
    _scratch: Option<tempfile::TempDir>,
}

impl DestStore {
    /// Opens the library at `files_dir/library.db`, or a throwaway unit on dry-run.
    ///
    /// Production uses the staged sqlite adapter (`Database.dropUnit` / connect
    /// stay in the guest). Tests of this crate use in-process host-helpers so
    /// they do not require a staged plugin tree.
    pub async fn open(files_dir: &Path, dry_run: bool) -> Result<Self> {
        #[cfg(test)]
        {
            let store = if dry_run {
                bookclerk_plugin_database_sqlite::open_store_memory().await?
            } else {
                std::fs::create_dir_all(files_dir)?;
                bookclerk_plugin_database_sqlite::open_store(&files_dir.join("library.db")).await?
            };
            Ok(Self {
                store,
                _scratch: None,
            })
        }
        #[cfg(not(test))]
        {
            open_via_adapter(files_dir, dry_run).await
        }
    }
}

/// Opens the staged sqlite adapter against `files_dir` (or a temp sqlite path on dry-run).
#[cfg(not(test))]
async fn open_via_adapter(files_dir: &Path, dry_run: bool) -> Result<DestStore> {
    use bookclerk_config::{Config, DatabaseConfig, Paths};

    let mut cfg = Config {
        paths: Some(Paths::from_files_dir(files_dir.to_path_buf())),
        database: DatabaseConfig {
            plugin: "sqlite".into(),
            ..DatabaseConfig::default()
        },
        ..Config::default()
    };
    let scratch = if dry_run {
        let tmp = tempfile::tempdir()?;
        cfg.database.sqlite.path = Some(tmp.path().join("library.db"));
        Some(tmp)
    } else {
        std::fs::create_dir_all(files_dir)?;
        None
    };
    let store = bookclerk_plugin_host::open_library_store_for_plugin(&cfg, "sqlite").await?;
    Ok(DestStore {
        store,
        _scratch: scratch,
    })
}
