//! `bookclerk migrate` — import classic Libation Files.

use std::path::PathBuf;

use bookclerk_config::Config;
use bookclerk_migrate::{migrate, MigrateOptions};
use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub enum MigrateCommand {
    /// Import Settings.json, AccountsSettings.json, and LibationContext.db.
    ///
    /// Point `--from` at your classic Libation Files directory (the folder that
    /// contains `Settings.json` / `AccountsSettings.json` / `LibationContext.db`).
    Import {
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

pub async fn run(command: MigrateCommand, config: &Config) -> anyhow::Result<()> {
    match command {
        MigrateCommand::Import {
            from,
            force,
            skip_auth,
            dry_run,
        } => {
            let dest = config.paths().files_dir.clone();
            eprintln!("migrating from {} → {}", from.display(), dest.display());
            let summary = migrate(MigrateOptions {
                source: from,
                dest_files_dir: dest,
                force,
                skip_auth,
                dry_run,
            })
            .await?;

            println!("settings\t{}", summary.settings_imported);
            println!("accounts\t{}", summary.accounts);
            println!("auth_files\t{}", summary.auth_files);
            println!("books\t{}", summary.books);
            println!("acquired\t{}", summary.acquired);
            println!("storage_keys\t{}", summary.storage_keys);
            for warning in &summary.warnings {
                eprintln!("warning: {warning}");
            }
            if dry_run {
                eprintln!("dry-run: no files written");
            }
            Ok(())
        }
    }
}
