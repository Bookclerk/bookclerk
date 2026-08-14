//! `bookclerk export` — native backup, Libation, library spreadsheet, Postgres.

use std::path::PathBuf;

use bookclerk_config::Config;
use bookclerk_migrate::{
    export_libation, export_native, LibationExportOptions, NativeExportOptions,
};
use clap::Subcommand;

use crate::commands::copydb::{self, CopyDbArgs, CopyDbFormat};
use crate::commands::export::{export_csv, export_json, export_xlsx, filter_books, load_books};
use crate::format_out::{emit, OutputFormat};

#[derive(Debug, Subcommand)]
/// `bookclerk export` subcommands: native backup, Libation, spreadsheet, or Postgres copy.
pub enum ExportCommand {
    /// Write a portable Bookclerk `.tar.gz` backup of the files directory.
    Native {
        /// Destination archive path.
        #[arg(short, long)]
        path: PathBuf,
        /// Include `plugins/**/plugin.toml` (not binaries).
        #[arg(long)]
        include_plugin_manifests: bool,
        /// Include `cache/` (large).
        #[arg(long)]
        include_cache: bool,
        /// Include `logs/`.
        #[arg(long)]
        include_logs: bool,
    },
    /// Export Settings.json, AccountsSettings.json, and LibationContext.db.
    Libation {
        /// Destination classic Libation Files directory.
        #[arg(short, long)]
        path: PathBuf,
        /// Overwrite an existing destination directory.
        #[arg(long)]
        force: bool,
        /// Report only; do not write.
        #[arg(long)]
        dry_run: bool,
    },
    /// Export library rows as CSV / JSON / XLSX.
    Library {
        /// Output file path.
        #[arg(short, long)]
        path: PathBuf,
        #[arg(long)]
        /// Force CSV even when the destination extension is not `.csv`.
        csv: bool,
        #[arg(long)]
        /// Force JSON even when the destination extension is not `.json`.
        json: bool,
        #[arg(long)]
        /// Force XLSX even when the destination extension is not `.xlsx`.
        xlsx: bool,
        /// Limit to specific ASINs / title ids.
        asins: Vec<String>,
        #[arg(long)]
        /// Limit exported library rows to this account id when set.
        account: Option<String>,
    },
    /// Copy library.db to PostgreSQL.
    Postgres {
        #[command(flatten)]
        /// Flattened `copydb` flags for the Postgres export path.
        args: CopyDbArgs,
    },
}

/// Dispatches an export subcommand and prints a human or JSON summary.
pub async fn run(
    command: ExportCommand,
    config: &Config,
    format: OutputFormat,
) -> anyhow::Result<()> {
    match command {
        ExportCommand::Native {
            path,
            include_plugin_manifests,
            include_cache,
            include_logs,
        } => {
            let summary = export_native(NativeExportOptions {
                files_dir: config.paths().files_dir.clone(),
                dest: path,
                bookclerk_version: env!("CARGO_PKG_VERSION").into(),
                include_plugin_manifests,
                include_cache,
                include_logs,
            })?;
            emit(format, &summary, || {
                println!("exported {} file(s) → {}", summary.files, summary.archive);
                for item in &summary.included {
                    println!("  {item}");
                }
            })
        }
        ExportCommand::Libation {
            path,
            force,
            dry_run,
        } => {
            let summary = export_libation(LibationExportOptions {
                files_dir: config.paths().files_dir.clone(),
                dest: path.clone(),
                force,
                dry_run,
            })
            .await?;
            emit(format, &summary, || {
                println!("dest\t{}", path.display());
                println!("settings\t{}", summary.settings);
                println!("accounts\t{}", summary.accounts);
                println!("books\t{}", summary.books);
                for w in &summary.warnings {
                    eprintln!("warning: {w}");
                }
                if dry_run {
                    eprintln!("dry-run: no files written");
                }
            })
        }
        ExportCommand::Library {
            path,
            csv,
            json,
            xlsx,
            asins,
            account,
        } => {
            let store = crate::registry::open_library(config).await?;
            let books = filter_books(
                load_books(&store, account.as_deref()).await?,
                if asins.is_empty() { None } else { Some(&asins) },
            );
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if csv || (!json && !xlsx && ext == "csv") {
                export_csv(&path, &books)?;
            } else if json || ext == "json" {
                export_json(&path, &books)?;
            } else if xlsx || ext == "xlsx" {
                export_xlsx(&path, &books)?;
            } else {
                export_csv(&path, &books)?;
            }
            let payload = serde_json::json!({
                "path": path.display().to_string(),
                "books": books.len(),
            });
            emit(format, &payload, || {
                println!("exported {} book(s) to {}", books.len(), path.display());
            })
        }
        ExportCommand::Postgres { args } => copydb::run(args, config).await,
    }
}

/// Default postgres format for the redesigned CLI (native flat schema).
#[allow(dead_code)]
pub fn default_postgres_format() -> CopyDbFormat {
    CopyDbFormat::Flat
}
