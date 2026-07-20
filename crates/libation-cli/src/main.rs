//! `libation` CLI — Phase 1 verb surface (LibationCli parity map).

mod commands;
mod progress;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use libation_config::{init_tracing, Config, LogFormat};

#[derive(Debug, Parser)]
#[command(
    name = "libation",
    version,
    about = "Headless Audible library manager (Libation Rust rewrite)",
    long_about = None
)]
struct Cli {
    /// Libation files directory (`LIBATION_FILES_DIR` / `--libationFiles` compat).
    #[arg(
        long = "libation-files",
        visible_alias = "libationFiles",
        env = "LIBATION_FILES_DIR",
        global = true
    )]
    libation_files: Option<PathBuf>,

    /// Path to config.toml (default: `{libation-files}/config.toml`).
    #[arg(long, env = "LIBATION_CONFIG", global = true)]
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
    /// Library scan, liberate, and status.
    Library {
        #[command(subcommand)]
        command: commands::library::LibraryCommand,
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
    let default_level = match cli.verbose {
        0 => "libation=info,warn",
        1 => "libation=debug,info",
        _ => "libation=trace,debug",
    };
    init_tracing(LogFormat::Text, default_level);

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("{err:#}");
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let config = Config::load(cli.libation_files.clone(), cli.config.clone())?;
    if let Some(paths) = &config.paths {
        paths.ensure_dirs()?;
    }

    match cli.command {
        Commands::Auth { command } => commands::auth::run(command, &config).await,
        Commands::Library { command } => commands::library::run(command, &config).await,
        Commands::Migrate { command } => commands::migrate::run(command, &config).await,
        Commands::Config { command } => commands::config_cmd::run(command, &config),
        Commands::CopyDb { args } => commands::copydb::run(args, &config).await,
        Commands::Version => {
            println!("libation {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}
