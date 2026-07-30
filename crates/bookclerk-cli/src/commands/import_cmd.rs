//! `bookclerk import` — native backup and Libation Files import.

use std::path::PathBuf;

use bookclerk_config::Config;
use bookclerk_migrate::{import_native, migrate, MigrateOptions, NativeImportOptions};
use clap::Subcommand;

use crate::format_out::{emit, OutputFormat};

#[derive(Debug, Subcommand)]
pub enum ImportCommand {
    /// Restore a native Bookclerk `.tar.gz` backup into the files directory.
    Native {
        /// Archive path.
        #[arg(long = "from", value_name = "ARCHIVE")]
        from: PathBuf,
        /// Overwrite existing files.
        #[arg(long)]
        force: bool,
        /// Report only; do not write.
        #[arg(long)]
        dry_run: bool,
    },
    /// Import classic Libation Files (Settings / accounts / DB).
    Libation {
        /// Classic Libation Files directory (or `BOOKCLERK_CLASSIC_FILES`).
        #[arg(long = "from", env = "BOOKCLERK_CLASSIC_FILES")]
        from: PathBuf,
        /// Overwrite existing `config.toml` and `.auth` files.
        #[arg(long)]
        force: bool,
        /// Import account metadata / library without writing `.auth` files.
        #[arg(long)]
        skip_auth: bool,
        /// Print what would be imported without writing.
        #[arg(long)]
        dry_run: bool,
    },
}

pub async fn run(
    command: ImportCommand,
    config: &Config,
    format: OutputFormat,
) -> anyhow::Result<()> {
    match command {
        ImportCommand::Native {
            from,
            force,
            dry_run,
        } => {
            let dest = config.paths().files_dir.clone();
            let summary = import_native(NativeImportOptions {
                archive: from,
                dest_files_dir: dest.clone(),
                force,
                dry_run,
            })?;
            emit(format, &summary, || {
                println!("dest\t{}", dest.display());
                println!("files\t{}", summary.files);
                println!("format_version\t{}", summary.format_version);
                for w in &summary.warnings {
                    eprintln!("warning: {w}");
                }
                if dry_run {
                    eprintln!("dry-run: no files written");
                }
            })
        }
        ImportCommand::Libation {
            from,
            force,
            skip_auth,
            dry_run,
        } => {
            let dest = config.paths().files_dir.clone();
            eprintln!(
                "importing libation from {} → {}",
                from.display(),
                dest.display()
            );
            let summary = migrate(MigrateOptions {
                source: from,
                dest_files_dir: dest,
                force,
                skip_auth,
                dry_run,
            })
            .await?;
            emit(format, &summary, || {
                println!("settings\t{}", summary.settings_imported);
                println!("accounts\t{}", summary.accounts);
                println!("credentials\t{}", summary.credentials);
                println!("books\t{}", summary.books);
                println!("acquired\t{}", summary.acquired);
                println!("storage_keys\t{}", summary.storage_keys);
                for warning in &summary.warnings {
                    eprintln!("warning: {warning}");
                }
                if dry_run {
                    eprintln!("dry-run: no files written");
                }
            })
        }
    }
}
