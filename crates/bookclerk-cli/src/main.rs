//! `bookclerk` CLI — Phase 1 verb surface (LibationCli parity map).

mod commands;
mod progress;
mod registry;

use std::path::PathBuf;
use std::process::ExitCode;

use bookclerk_config::{init_tracing_with, Config, LogFormat, TracingOptions};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "bookclerk",
    version,
    about = "Headless multi-source audiobook library manager",
    long_about = None
)]
struct Cli {
    /// Bookclerk files directory (`BOOKCLERK_FILES_DIR` / `--bookclerkFiles` compat).
    #[arg(
        long = "bookclerk-files",
        visible_alias = "bookclerkFiles",
        env = "BOOKCLERK_FILES_DIR",
        global = true
    )]
    bookclerk_files: Option<PathBuf>,

    /// Path to config.toml (default: `{bookclerk-files}/config.toml`).
    #[arg(long, env = "BOOKCLERK_CONFIG", global = true)]
    config: Option<PathBuf>,

    /// Increase logging verbosity (`-v`, `-vv`).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Authentication and account management.
    Auth {
        #[command(subcommand)]
        command: commands::auth::AuthCommand,
    },
    /// Library scan, acquire, and status.
    Library {
        #[command(subcommand)]
        command: commands::library::LibraryCommand,
    },
    /// Outbound integrations (Audiobookshelf) and claim tickets.
    Integrations {
        #[command(subcommand)]
        command: commands::integrations::IntegrationsCommand,
    },
    /// Dynamically discovered third-party plugins.
    Plugins {
        #[command(subcommand)]
        command: commands::plugins::PluginsCommand,
    },
    /// Import classic Libation Files (Settings / accounts / DB).
    Migrate {
        #[command(subcommand)]
        command: commands::migrate::MigrateCommand,
    },
    /// Read configuration values.
    Config {
        #[command(subcommand)]
        command: commands::config_cmd::ConfigCommand,
    },
    /// Copy library.db to PostgreSQL (LibationCli: `copydb`).
    CopyDb {
        #[command(flatten)]
        args: commands::copydb::CopyDbArgs,
    },
    /// Print version information (LibationCli: `version`).
    Version,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let config = match Config::load(cli.bookclerk_files.clone(), cli.config.clone()) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("error: {err:#}");
            return ExitCode::FAILURE;
        }
    };

    let default_level = match cli.verbose {
        0 => "bookclerk=info,warn",
        1 => "bookclerk=debug,info",
        _ => "bookclerk=trace,debug",
    };
    init_tracing_with(TracingOptions {
        format: LogFormat::Text,
        default_level: default_level.to_string(),
        syslog_identifier: "bookclerk".into(),
        diagnostics: config.diagnostics.clone(),
        version: env!("CARGO_PKG_VERSION").into(),
        enable_journald: true,
    });
    // After subscriber install so startup guidance is not dropped.
    config.warn_unsupported_options();

    match run(cli, config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("{err:#}");
            eprintln!("error: {err:#}");
            if let Some(diag) = bookclerk_config::diagnostics_global() {
                // Blocking: process exit must not kill a background upload thread.
                diag.upload_blocking("cli_error");
            }
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli, config: Config) -> anyhow::Result<()> {
    bookclerk_audible::configure_auth_secrets(
        config.auth.password_file.clone(),
        config.auth.allow_plaintext,
    );
    if let Some(paths) = &config.paths {
        paths.ensure_dirs()?;
    }

    match cli.command {
        Commands::Auth { command } => commands::auth::run(command, &config).await,
        Commands::Library { command } => commands::library::run(command, &config).await,
        Commands::Integrations { command } => commands::integrations::run(command, &config).await,
        Commands::Plugins { command } => commands::plugins::run(command, &config).await,
        Commands::Migrate { command } => commands::migrate::run(command, &config).await,
        Commands::Config { command } => commands::config_cmd::run(command, &config),
        Commands::CopyDb { args } => commands::copydb::run(args, &config).await,
        Commands::Version => {
            println!("bookclerk {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}
