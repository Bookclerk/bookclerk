//! `bookclerk migrate` — hidden alias of `import libation`.

use std::path::PathBuf;

use bookclerk_config::Config;
use clap::Subcommand;

use crate::commands::import_cmd::{self, ImportCommand};
use crate::format_out::OutputFormat;

#[derive(Debug, Subcommand)]
pub enum MigrateCommand {
    /// Import Settings.json, AccountsSettings.json, and LibationContext.db.
    ///
    /// Prefer `bookclerk import libation`.
    Import {
        /// Classic Libation Files directory (or `BOOKCLERK_CLASSIC_FILES`).
        #[arg(long = "from", env = "BOOKCLERK_CLASSIC_FILES")]
        from: PathBuf,
        /// Overwrite existing `config.toml`.
        #[arg(long)]
        force: bool,
        /// No-op retained for compatibility (credentials are never imported by migrate).
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
            import_cmd::run(
                ImportCommand::Libation {
                    from,
                    force,
                    skip_auth,
                    dry_run,
                },
                config,
                OutputFormat::Text,
            )
            .await
        }
    }
}
