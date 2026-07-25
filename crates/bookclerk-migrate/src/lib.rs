//! Migrate data from classic (C#) Libation into Bookclerk.

mod accounts;
mod discover;
mod error;
mod files;
mod library;
mod settings;

pub use accounts::import_accounts;
pub use discover::{discover_source, ClassicSource};
pub use error::{MigrateError, Result};
pub use files::load_audio_paths;
pub use library::import_library_db;
pub use settings::{apply_settings_json, load_settings_json};

use std::collections::HashMap;
use std::path::Path;

use bookclerk_config::Config;
use bookclerk_library::LibraryStore;
use serde::{Deserialize, Serialize};

/// Options for a full classic Libation Files directory import.
#[derive(Debug, Clone)]
pub struct MigrateOptions {
    /// Source classic Libation Files directory.
    pub source: std::path::PathBuf,
    /// Destination files dir (auth, library.db, config.toml).
    pub dest_files_dir: std::path::PathBuf,
    /// Overwrite existing auth files / config.toml.
    pub force: bool,
    /// Skip writing `.auth` files (still upserts account metadata).
    pub skip_auth: bool,
    /// Report only; do not write.
    pub dry_run: bool,
}

/// Summary of a migration run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MigrateSummary {
    pub settings_imported: bool,
    pub accounts: usize,
    pub auth_files: usize,
    pub books: usize,
    pub acquired: usize,
    pub storage_keys: usize,
    pub warnings: Vec<String>,
}

/// Import classic Libation Files into a bookclerk data directory.
pub async fn migrate(opts: MigrateOptions) -> Result<MigrateSummary> {
    let source = discover_source(&opts.source)?;
    let mut summary = MigrateSummary::default();

    if !opts.dry_run {
        std::fs::create_dir_all(&opts.dest_files_dir)?;
        let _ = bookclerk_audible::ensure_accounts_dir(&opts.dest_files_dir);
    }

    // --- Settings.json → config.toml ---
    let mut config = Config::default();
    if let Some(settings_path) = &source.settings_json {
        let patch = load_settings_json(settings_path)?;
        apply_settings_json(&mut config, &patch);
        summary.settings_imported = true;
        if !opts.dry_run {
            let dest = opts.dest_files_dir.join("config.toml");
            if dest.exists() && !opts.force {
                summary.warnings.push(format!(
                    "config.toml already exists at {} (pass --force to overwrite)",
                    dest.display()
                ));
            } else {
                config.write_toml_file(&dest)?;
            }
        }
    } else {
        summary
            .warnings
            .push("Settings.json not found — using defaults".into());
    }

    let books_root = config.output.local.root.clone();

    // --- AccountsSettings.json ---
    let mut account_id_map = HashMap::new();
    if let Some(accounts_path) = &source.accounts_settings {
        let acct = import_accounts(
            accounts_path,
            &opts.dest_files_dir,
            opts.force,
            opts.skip_auth,
            opts.dry_run,
        )
        .await?;
        summary.accounts = acct.accounts;
        summary.auth_files = acct.auth_files;
        summary.warnings.extend(acct.warnings);
        account_id_map = acct.account_id_map;
    } else {
        summary
            .warnings
            .push("AccountsSettings.json not found — accounts not imported".into());
    }

    // --- FileLocationsV2.json (optional) ---
    let audio_paths = if let Some(path) = &source.file_locations {
        load_audio_paths(path)?
    } else {
        Default::default()
    };

    // --- LibationContext.db ---
    if let Some(db_path) = &source.library_db {
        let library_db = opts.dest_files_dir.join("library.db");
        let store = if opts.dry_run {
            LibraryStore::open_in_memory()?
        } else {
            LibraryStore::open(&library_db)?
        };
        let lib = import_library_db(
            db_path,
            &store,
            &audio_paths,
            books_root.as_path(),
            &account_id_map,
            opts.dry_run,
        )?;
        summary.books = lib.books;
        summary.acquired = lib.acquired;
        summary.storage_keys = lib.storage_keys;
        summary.warnings.extend(lib.warnings);
    } else {
        summary
            .warnings
            .push("LibationContext.db not found — library not imported".into());
    }

    Ok(summary)
}

/// Resolve a classic Libation path from CLI / env.
pub fn resolve_classic_files_dir(explicit: Option<&Path>) -> Option<std::path::PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }
    if let Ok(path) = std::env::var("BOOKCLERK_CLASSIC_FILES") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return Some(std::path::PathBuf::from(trimmed));
        }
    }
    None
}
