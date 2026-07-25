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
use crate::registry::default_registry_with_plugins;
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
    let mut integrations = libation_integrations::from_config(&config)?;
    libation_plugin::load_external_integrations(&config, &mut integrations).await?;
    let sources = {
        let cfg = config.clone();
        let reg = default_registry_with_plugins(&cfg).await?;
        reg.all()
    };
    let config = Arc::new(RwLock::new(config));
    let state = Arc::new(AppState {
        config: config.clone(),
        library: library.clone(),
        jobs: Arc::new(RwLock::new(Vec::new())),
        work_lock: Mutex::new(()),
        integrations: integrations.clone(),
        sources,
    });

    // Start integration watchers; mint claim tickets on new ABS users.
    {
        let library_for_tickets = library.clone();
        let config_for_tickets = config.clone();
        let ctx = libation_integrations::IntegrationContext {
            on_external_user: Some(Arc::new(move |user| {
                let library_for_tickets = library_for_tickets.clone();
                let config_for_tickets = config_for_tickets.clone();
                tokio::spawn(async move {
                    let cfg = config_for_tickets.read().await;
                    match libation_integrations::mint_for_external_user(
                        &library_for_tickets,
                        &cfg,
                        &user,
                        "abs_watcher",
                    ) {
                        Ok(minted) => {
                            if let Some(url) = minted.portal_url {
                                tracing::info!(%url, "claim ticket minted for ABS user");
                            } else {
                                tracing::info!(
                                    token = %minted.token,
                                    "claim ticket minted for ABS user"
                                );
                            }
                        }
                        Err(err) => tracing::warn!(%err, "failed to mint claim ticket"),
                    }
                });
            })),
        };
        let integrations = integrations.clone();
        tokio::spawn(async move {
            let _ = integrations.start_all(ctx).await;
        });
    }

    spawn_scheduler(state.clone());

    let cfg_snapshot = config.read().await;
    let listen = cfg_snapshot.daemon.listen.clone();
    let portal_base =
        libation_integrations::normalize_portal_base(&cfg_snapshot.integrations.portal_base_path);
    let files_dir = cfg_snapshot.paths().files_dir.clone();
    drop(cfg_snapshot);

    let addr: SocketAddr = listen
        .parse()
        .map_err(|err| anyhow::anyhow!("invalid daemon.listen '{listen}': {err}"))?;

    // Ensure portal state files_dir is set before nest.
    let app = {
        // Rebuild sources/files into router via helper that takes portal_base + files_dir
        router(state, portal_base, files_dir)
    };
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
