//! `bookclerkd` — long-running scan / acquire daemon with HTTP control plane.

mod api;
mod auth;
mod jobs;
mod registry;
mod scheduler;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use bookclerk_config::{
    init_tracing_with, read_or_create_operator_token, Config, LogFormat, TracingOptions,
};
use bookclerk_library::{configure_master_key_with, LibraryStore};
use clap::Parser;
use tokio::sync::{Mutex, RwLock};

use crate::api::{reload_daemon_config, resolve_ui_dist, router, AppState};
use crate::auth::OperatorAuthState;
use crate::registry::default_registry_with_plugins;
use crate::scheduler::spawn_scheduler;

#[derive(Debug, Parser)]
#[command(name = "bookclerkd", version, about = "Bookclerk background daemon")]
struct Args {
    /// Bookclerk files directory.
    #[arg(
        long = "bookclerk-files",
        visible_alias = "bookclerkFiles",
        env = "BOOKCLERK_FILES_DIR"
    )]
    bookclerk_files: Option<PathBuf>,

    /// Path to config.toml.
    #[arg(long, env = "BOOKCLERK_CONFIG")]
    config: Option<PathBuf>,

    /// Override HTTP listen address.
    #[arg(long, env = "BOOKCLERK_DAEMON_LISTEN")]
    listen: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mut config = Config::load(args.bookclerk_files, args.config)?;
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
        default_level: "bookclerk=info,warn".into(),
        syslog_identifier: "bookclerkd".into(),
        diagnostics: config.diagnostics.clone(),
        version: env!("CARGO_PKG_VERSION").into(),
        enable_journald: true,
    });
    // After subscriber install so startup guidance is not dropped.
    config.warn_unsupported_options();
    // Before any acquire can start, so codec work never runs unconfined.
    bookclerk_media::init_pool_from_config(&config.media);
    if logging.journald {
        let facility = match logging.os_facility {
            Some(bookclerk_config::OsLogFacility::Journald) => "journald",
            Some(bookclerk_config::OsLogFacility::OsLog) => "os_log",
            Some(bookclerk_config::OsLogFacility::EventLog) => "windows-event-log",
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
    configure_master_key_with(&paths.files_dir, config.auth_password().as_deref())?;

    let library = LibraryStore::open_from_config(&config).await?;
    let integrations = bookclerk_plugin::load_integrations(&config).await?;
    let destinations =
        bookclerk_plugin::load_external_destinations(&config, Some(library.db())).await?;
    let sources = {
        let cfg = config.clone();
        default_registry_with_plugins(&cfg).await?
    };
    let auth_cfg = {
        let listen = config.daemon.listen.clone();
        let auth = config.daemon.auth.clone();
        if !auth.enabled && !listen_is_loopback(&listen) {
            anyhow::bail!(
                "daemon.auth.enabled=false is unsafe when listen is not loopback ({listen}); \
                 enable operator auth or bind 127.0.0.1"
            );
        }
        auth
    };
    let operator_auth = if auth_cfg.enabled {
        let (token, _created) = read_or_create_operator_token(&config)?;
        Some(Arc::new(OperatorAuthState::new(
            token,
            auth_cfg.session_ttl_hours,
            true,
        )))
    } else {
        tracing::warn!("daemon.auth.enabled=false — HTTP API is unauthenticated");
        Some(Arc::new(OperatorAuthState::new(
            String::new(),
            auth_cfg.session_ttl_hours,
            false,
        )))
    };

    let config = Arc::new(RwLock::new(config));
    let state = Arc::new(AppState {
        config: config.clone(),
        library: library.clone(),
        jobs: Arc::new(RwLock::new(Vec::new())),
        work_lock: Mutex::new(()),
        integrations: integrations.clone(),
        sources,
        destinations: Arc::new(RwLock::new(destinations)),
        auth: operator_auth,
    });

    // Start integration watchers; mint claim tickets on new ABS users.
    {
        let library_for_tickets = library.clone();
        let config_for_tickets = config.clone();
        let ctx = bookclerk_integrations::IntegrationContext {
            on_external_user: Some(Arc::new(move |user| {
                let library_for_tickets = library_for_tickets.clone();
                let config_for_tickets = config_for_tickets.clone();
                tokio::spawn(async move {
                    let cfg = config_for_tickets.read().await;
                    match bookclerk_integrations::mint_for_external_user(
                        &library_for_tickets,
                        &cfg,
                        &user,
                        "abs_watcher",
                    )
                    .await
                    {
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
    spawn_config_reload_signals(state.clone());

    let cfg_snapshot = config.read().await;
    let listen = cfg_snapshot.daemon.listen.clone();
    drop(cfg_snapshot);

    let addr: SocketAddr = listen
        .parse()
        .map_err(|err| anyhow::anyhow!("invalid daemon.listen '{listen}': {err}"))?;

    let ui_dist = resolve_ui_dist();
    let app = router(state, ui_dist);
    tracing::info!(%addr, "bookclerkd listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

fn listen_is_loopback(listen: &str) -> bool {
    let host = listen
        .strip_prefix("http://")
        .or_else(|| listen.strip_prefix("https://"))
        .unwrap_or(listen);
    let host = host.split(':').next().unwrap_or(host);
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

fn spawn_config_reload_signals(state: Arc<AppState>) {
    #[cfg(unix)]
    {
        tokio::spawn(async move {
            let mut stream =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
                    Ok(s) => s,
                    Err(err) => {
                        tracing::warn!(%err, "failed to install SIGHUP handler for config reload");
                        return;
                    }
                };
            while stream.recv().await.is_some() {
                match reload_daemon_config(&state).await {
                    Ok(detail) => tracing::info!(%detail, "SIGHUP config reload"),
                    Err(err) => tracing::error!(error = %err, "SIGHUP config reload failed"),
                }
            }
        });
    }
    #[cfg(not(unix))]
    {
        let _ = state;
    }
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
