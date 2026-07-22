//! `libationd` — long-running scan / liberate daemon with HTTP control plane.

mod api;
mod jobs;
mod registry;
mod scheduler;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use libation_config::{init_tracing_with, Config, LogFormat, TracingOptions};
use libation_library::LibraryStore;
use tokio::sync::{Mutex, RwLock};

use crate::api::{router, AppState};
use crate::scheduler::spawn_scheduler;

#[derive(Debug, Parser)]
#[command(name = "libationd", version, about = "Libation background daemon")]
struct Args {
    /// Libation files directory.
    #[arg(
        long = "libation-files",
        visible_alias = "libationFiles",
        env = "LIBATION_FILES_DIR"
    )]
    libation_files: Option<PathBuf>,

    /// Path to config.toml.
    #[arg(long, env = "LIBATION_CONFIG")]
    config: Option<PathBuf>,

    /// Override HTTP listen address.
    #[arg(long, env = "LIBATION_DAEMON_LISTEN")]
    listen: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mut config = Config::load(args.libation_files, args.config)?;
    libation_audible::configure_auth_secrets(
        config.auth.password_file.clone(),
        config.auth.allow_plaintext,
    );
    if let Some(listen) = args.listen {
        config.daemon.listen = listen;
    }

    let log_format = if config.daemon.json_logs {
        LogFormat::Json
    } else {
        LogFormat::Text
    };
    let logging = init_tracing_with(TracingOptions {
        format: log_format,
        default_level: "libation=info,warn".into(),
        syslog_identifier: "libationd".into(),
        diagnostics: config.diagnostics.clone(),
        version: env!("CARGO_PKG_VERSION").into(),
        enable_journald: true,
    });
    // After subscriber install so startup guidance is not dropped.
    config.warn_unsupported_options();
    if logging.journald {
        let facility = match logging.os_facility {
            Some(libation_config::OsLogFacility::Journald) => "journald",
            Some(libation_config::OsLogFacility::OsLog) => "os_log",
            Some(libation_config::OsLogFacility::EventLog) => "windows-event-log",
            None => "os",
        };
        tracing::info!(%facility, "OS log facility enabled (structured system logging)");
    } else {
        tracing::info!("OS log facility unavailable; logging to stderr only");
    }
    if config.diagnostics.share_reports {
        tracing::info!(
            url = %config.diagnostics.effective_submit_url(),
            "diagnostics.share_reports=true — redacted reports POST to Worker /submit (B2 via Cloudflare)"
        );
    }

    let paths = config.paths().clone();
    paths.ensure_dirs()?;

    let library = LibraryStore::open(&paths.library_db)?;
    let state = Arc::new(AppState {
        config: RwLock::new(config.clone()),
        library,
        jobs: Arc::new(RwLock::new(Vec::new())),
        work_lock: Mutex::new(()),
    });

    spawn_scheduler(state.clone());

    let addr: SocketAddr = config.daemon.listen.parse().map_err(|err| {
        anyhow::anyhow!("invalid daemon.listen '{}': {err}", config.daemon.listen)
    })?;

    let app = router(state);
    tracing::info!(%addr, "libationd listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
