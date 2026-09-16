//! `bookclerk` CLI — Bookclerk-native headless library manager.

mod cli_plugin;
/// Subcommand implementations (`library`, `discover`, `daemon`, …).
mod commands;
mod format_out;
mod progress;
/// CLI helpers that open the library store and source/integration registries.
mod registry;

use std::path::PathBuf;
use std::process::ExitCode;

use bookclerk_config::{init_tracing_with, Config, LogFormat, TracingOptions};
use bookclerk_library::configure_master_key_with;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};

use crate::cli_plugin::RESERVED_PLUGIN_SUBCOMMANDS;
use crate::commands::plugins::augment_plugins_command;
use crate::format_out::OutputFormat;

#[derive(Debug, Parser)]
#[command(
    name = "bookclerk",
    version,
    about = "Headless multi-source audiobook library manager",
    long_about = None
)]
/// Top-level `bookclerk` clap parser (files dir, config, verbosity, output format).
struct Cli {
    /// Bookclerk files directory (`BOOKCLERK_FILES_DIR`).
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

    /// Increase tracing verbosity (`-v` warnings, `-vv` debug, `-vvv` trace).
    /// Default is quiet: command output only (override with `BOOKCLERK_LOG`).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    /// Machine-readable output for list/status style commands.
    #[arg(long, value_enum, global = true, default_value_t = OutputFormat::Text)]
    format: OutputFormat,

    #[command(subcommand)]
    /// Selected top-level verb (`library`, `discover`, `daemon`, …).
    command: Commands,
}

#[derive(Debug, Subcommand)]
/// Top-level `bookclerk` verbs; hidden aliases keep classic Libation command names.
enum Commands {
    /// Library scan, acquire, search, accounts, and status.
    Library {
        #[command(subcommand)]
        /// Nested `library` verb (scan, acquire, search, accounts).
        command: commands::library::LibraryCommand,
    },
    /// Recommendations, embeddings, listening sync, and title requests.
    Discover {
        #[command(subcommand)]
        /// Nested `discover` verb (recommend, embed, wishlist).
        command: commands::discover::DiscoverCommand,
    },
    /// Outbound integrations (Audiobookshelf) and claim tickets.
    Integrations {
        #[command(subcommand)]
        /// Nested `integrations` verb (Audiobookshelf, claim tickets).
        command: commands::integrations::IntegrationsCommand,
    },
    /// Dynamically discovered third-party plugins.
    Plugins {
        #[command(subcommand)]
        /// Nested `plugins` verb, including dynamically discovered plugin ids.
        command: commands::plugins::PluginsCommand,
    },
    /// Read or write configuration values.
    Config {
        #[command(subcommand)]
        /// Nested `config` verb (get/set keys, master-key wrap, S3 credentials).
        command: commands::config_cmd::ConfigCommand,
    },
    /// Export library data, backups, or Libation-compatible files.
    Export {
        #[command(subcommand)]
        /// Nested `export` verb (library dump, backups, Postgres copy).
        command: commands::export_cmd::ExportCommand,
    },
    /// Import native backups or classic Libation Files.
    Import {
        #[command(subcommand)]
        /// Nested `import` verb (native backups or classic Libation Files).
        command: commands::import_cmd::ImportCommand,
    },
    /// Host schema version, backups, migrate, and last-reversible downgrade.
    Db {
        #[command(subcommand)]
        /// Nested `db` verb (version, backup, restore, migrate, downgrade).
        command: commands::db::DbCommand,
    },
    /// Talk to a running bookclerkd control plane.
    Daemon {
        #[command(subcommand)]
        /// Nested `daemon` verb (health, jobs, operator token).
        command: commands::daemon_cmd::DaemonCommand,
    },
    /// Durable domain-event outbox and deliveries.
    Events {
        #[command(subcommand)]
        /// Nested `events` verb (list, dead-letters, retry, ack, cancel, resume).
        command: commands::events::EventsCommand,
    },
    /// Print a loopback operator sign-in URL (requires a running daemon).
    Login,
    /// Diagnostics ring buffer and opt-in upload.
    Diagnostics {
        #[command(subcommand)]
        /// Nested `diagnostics` verb (ring buffer dump, opt-in upload).
        command: commands::diagnostics_cmd::DiagnosticsCommand,
    },
    /// Import classic Libation Files (alias of `import libation`).
    #[command(hide = true)]
    Migrate {
        #[command(subcommand)]
        /// Hidden `migrate` alias forwarded to `import libation`.
        command: commands::migrate::MigrateCommand,
    },
    /// Copy library.db to PostgreSQL (alias of `export postgres`).
    #[command(hide = true, name = "copydb")]
    CopyDb {
        #[command(flatten)]
        /// Hidden `copydb` alias forwarded to `export postgres`.
        args: commands::copydb::CopyDbArgs,
    },
    /// Print version information.
    Version,
}

#[tokio::main]
async fn main() -> ExitCode {
    // Phase 1: resolve files dir / config for plugin discovery (help + dynamic cmds).
    let early = Cli::command()
        .ignore_errors(true)
        .try_get_matches_from(std::env::args_os());
    let (files, config_path, verbose) = match &early {
        Ok(m) => (
            m.get_one::<PathBuf>("bookclerk_files").cloned(),
            m.get_one::<PathBuf>("config").cloned(),
            m.get_count("verbose"),
        ),
        Err(_) => (None, None, 0),
    };

    let config = match Config::load(files, config_path) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("error: {err:#}");
            return ExitCode::FAILURE;
        }
    };

    let default_level = match verbose {
        0 => "off",
        1 => "warn",
        2 => "bookclerk=debug,info",
        _ => "bookclerk=trace,debug",
    };
    let _logging = init_tracing_with(TracingOptions {
        format: LogFormat::Text,
        default_level: default_level.to_string(),
        syslog_identifier: "bookclerk".into(),
        diagnostics: config.diagnostics.clone(),
        version: env!("CARGO_PKG_VERSION").into(),
        enable_journald: true,
    });
    // Keep the non-blocking stderr worker alive for the process lifetime.
    let _stderr_guard = &_logging;
    config.warn_unsupported_options();
    if verbose == 0 {
        print_cli_setup_warnings(&config);
    }
    // Before any acquire can start, so codec work never runs unconfined.
    bookclerk_media::init_pool_from_config(&config.media);

    let format = early
        .as_ref()
        .ok()
        .and_then(|m| m.get_one::<OutputFormat>("format").copied())
        .unwrap_or_default();

    // Dynamic plugin CLI: `bookclerk plugins <plugin-id> …`
    if let Some((plugin_id, rest)) = plugin_cli_args(&std::env::args().collect::<Vec<_>>()) {
        if let Some(paths) = &config.paths {
            let _ = paths.ensure_dirs();
            let _ = configure_master_key_with(&paths.files_dir, config.auth_password().as_deref());
        }
        return match commands::plugins::run_plugin_cli(&config, plugin_id, rest, format).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                tracing::error!("{err:#}");
                eprintln!("error: {err:#}");
                if let Some(diag) = bookclerk_config::diagnostics_global() {
                    diag.upload_blocking("cli_error");
                }
                ExitCode::FAILURE
            }
        };
    }

    let mut cmd = build_cli(&config);
    let matches = match cmd.try_get_matches_from_mut(std::env::args_os()) {
        Ok(m) => m,
        Err(err) => {
            let _ = err.print();
            return if err.use_stderr() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            };
        }
    };

    let cli = match Cli::from_arg_matches(&matches) {
        Ok(c) => c,
        Err(err) => {
            let _ = err.print();
            return ExitCode::FAILURE;
        }
    };

    match run(cli, config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("{err:#}");
            eprintln!("error: {err:#}");
            if let Some(diag) = bookclerk_config::diagnostics_global() {
                diag.upload_blocking("cli_error");
            }
            ExitCode::FAILURE
        }
    }
}

/// Builds the clap command tree, injecting dynamically discovered plugin subcommands.
fn build_cli(config: &Config) -> clap::Command {
    let mut cmd = Cli::command();
    if let Some(plugins_cmd) = cmd.find_subcommand_mut("plugins") {
        let augmented = augment_plugins_command(plugins_cmd.clone(), config);
        *plugins_cmd = augmented;
    }
    cmd
}

/// Detect `plugins <plugin-id> …` where `<plugin-id>` is not a host verb.
fn plugin_cli_args(argv: &[String]) -> Option<(&str, &[String])> {
    // argv[0] = binary
    let mut i = 1;
    while i < argv.len() {
        let a = argv[i].as_str();
        if a == "plugins" {
            let id = argv.get(i + 1)?.as_str();
            if RESERVED_PLUGIN_SUBCOMMANDS
                .iter()
                .any(|r| r.eq_ignore_ascii_case(id))
            {
                return None;
            }
            if id.starts_with('-') {
                return None;
            }
            return Some((id, &argv[i + 2..]));
        }
        // Skip global flags that take a value.
        if matches!(
            a,
            "--bookclerk-files" | "--bookclerkFiles" | "--config" | "--format"
        ) {
            i += 2;
            continue;
        }
        if a.starts_with("--bookclerk-files=")
            || a.starts_with("--bookclerkFiles=")
            || a.starts_with("--config=")
            || a.starts_with("--format=")
            || a == "-v"
            || a == "-vv"
            || a == "-vvv"
            || a.starts_with("-v")
        {
            i += 1;
            continue;
        }
        // First non-global token that isn't plugins → not a plugin forward.
        if !a.starts_with('-') {
            return None;
        }
        i += 1;
    }
    None
}

/// Human-facing setup warnings for default (quiet) CLI verbosity.
///
/// Tracing is `off` at `-v` count 0 so command tables stay readable; these
/// lines replace the daemon-style `tracing::warn!` startup notes.
fn print_cli_setup_warnings(config: &Config) {
    if config.auth_password().is_none() {
        eprintln!(
            "warning: no auth password set — master.key may be BCK1 (unwrapped DEK). \
             Set BOOKCLERK_AUTH_PASSWORD or [auth].password, then \
             `bookclerk config master-key wrap`."
        );
    }
}

/// Dispatches the parsed verb after ensuring files-dir layout and the master key.
async fn run(cli: Cli, config: Config) -> anyhow::Result<()> {
    if let Some(paths) = &config.paths {
        paths.ensure_dirs()?;
        configure_master_key_with(&paths.files_dir, config.auth_password().as_deref())?;
    }
    let format = cli.format;

    match cli.command {
        Commands::Library { command } => commands::library::run(command, &config).await,
        Commands::Discover { command } => commands::discover::run(&config, format, command).await,
        Commands::Integrations { command } => commands::integrations::run(command, &config).await,
        Commands::Plugins { command } => commands::plugins::run(command, &config, format).await,
        Commands::Config { command } => commands::config_cmd::run(command, &config, format).await,
        Commands::Export { command } => commands::export_cmd::run(command, &config, format).await,
        Commands::Import { command } => commands::import_cmd::run(command, &config, format).await,
        Commands::Db { command } => commands::db::run(command, &config, format).await,
        Commands::Daemon { command } => commands::daemon_cmd::run(command, &config, format).await,
        Commands::Events { command } => commands::events::run(command, &config, format).await,
        Commands::Login => commands::daemon_cmd::run_login(&config, format).await,
        Commands::Diagnostics { command } => {
            commands::diagnostics_cmd::run(command, &config, format)
        }
        Commands::Migrate { command } => commands::migrate::run(command, &config).await,
        Commands::CopyDb { args } => commands::copydb::run(args, &config).await,
        Commands::Version => {
            let payload = serde_json::json!({ "version": env!("CARGO_PKG_VERSION") });
            crate::format_out::emit(format, &payload, || {
                println!("bookclerk {}", env!("CARGO_PKG_VERSION"));
            })
        }
    }
}
