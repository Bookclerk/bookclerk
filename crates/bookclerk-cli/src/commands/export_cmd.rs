//! `bookclerk export` — native backup, Libation, library spreadsheet, Postgres.

use std::path::{Path, PathBuf};

use bookclerk_config::Config;
use bookclerk_migrate::{
    export_libation, export_native, write_classic_storefront_file, LibationExportOptions,
    LibationExportSummary, NativeExportOptions,
};
use clap::Subcommand;

use crate::commands::copydb::{self, CopyDbArgs, CopyDbFormat};
use crate::commands::export::{load_rows, write_export};
use crate::format_out::{emit, line, OutputFormat};

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
        /// Include `plugin-databases/` (SQLite binding files).
        #[arg(long)]
        include_plugin_databases: bool,
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
        /// Write this person's tags, ratings and finished flags (login name
        /// or user id); without it they are empty.
        #[arg(long)]
        user: Option<String>,
    },
    /// Export library titles as CSV / JSON / XLSX: the whole library, or
    /// with `--user` that person's view with their tags and finished flags.
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
        #[arg(long)]
        /// Fill `tags` and `is_finished` with this person's own state (login
        /// name or user id); without it they are empty.
        user: Option<String>,
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
            include_plugin_databases,
        } => {
            let summary = export_native(NativeExportOptions {
                files_dir: config.paths().files_dir.clone(),
                dest: path,
                bookclerk_version: env!("CARGO_PKG_VERSION").into(),
                include_plugin_manifests,
                include_cache,
                include_logs,
                include_plugin_databases,
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
            user,
        } => {
            let summary = export_libation(LibationExportOptions {
                files_dir: config.paths().files_dir.clone(),
                dest: path.clone(),
                force,
                dry_run,
                user,
            })
            .await?;
            if !dry_run {
                write_classic_storefront_file(&config.paths().files_dir, &path).await?;
            }
            emit_libation_export(format, &path, &summary, dry_run)
        }
        ExportCommand::Library {
            path,
            csv,
            json,
            xlsx,
            asins,
            account,
            user,
        } => {
            let store = crate::registry::open_library(config).await?;
            let user = match user {
                Some(user) => Some(crate::commands::nodes::resolve_user(&store, &user).await?),
                None => None,
            };
            let rows = load_rows(&store, account.as_deref(), &asins, user.as_ref()).await?;
            write_export(&path, &rows, csv, json, xlsx)?;
            let payload = serde_json::json!({
                "path": path.display().to_string(),
                "books": rows.len(),
            });
            emit(format, &payload, || {
                println!("exported {} book(s) to {}", rows.len(), path.display());
            })
        }
        ExportCommand::Postgres { args } => copydb::run(args, config).await,
    }
}

/// Prints a Libation export summary without reading identity rows.
fn emit_libation_export(
    format: OutputFormat,
    dest: &Path,
    summary: &LibationExportSummary,
    dry_run: bool,
) -> anyhow::Result<()> {
    let n_storefronts = summary.storefronts;
    let n_books = summary.books;
    let notes = summary.warnings.clone();
    emit(format, summary, || {
        line(format!("dest\t{}", dest.display()));
        line(format!("settings\t{}", summary.settings));
        line(format!("accounts\t{n_storefronts}"));
        line(format!("books\t{n_books}"));
        for note in &notes {
            eprintln!("warning: {note}");
        }
        if dry_run {
            eprintln!("dry-run: no files written");
        }
    })
}

/// Default postgres format for the redesigned CLI (native flat schema).
#[allow(dead_code)]
pub fn default_postgres_format() -> CopyDbFormat {
    CopyDbFormat::Flat
}
