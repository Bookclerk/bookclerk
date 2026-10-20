//! `bookclerkd` — long-running scan / acquire daemon with HTTP control plane.

// Axum `Response` as handler `Err` exceeds Clippy's 128-byte `result_large_err` cap.
#![allow(clippy::result_large_err)]

mod api;
mod auth;
mod cluster_link;
mod config_authority;
mod csrf;
mod event_worker;
mod heartbeat;
mod http_error;
mod job_handler;
mod job_worker;
mod jobs;
mod live;
mod maintenance;
mod node_api;
mod node_queue;
mod oidc;
mod oidc_rp;
mod oidc_verify;
mod origin;
mod passkeys;
mod placement;
mod plugin_installs;
mod profile;
/// Builds the daemon source / integration registry for [`AppState`].
mod registry;
mod requirements_sweep;
mod scheduler;
mod totp;
mod tray_companion;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bookclerk_config::{init_tracing_with, Config, ListenAddrs, LogFormat, TracingOptions};
use clap::Parser;
use tokio::sync::{Mutex, Notify, RwLock, Semaphore};

use crate::api::{
    bind_listen_addrs, build_operator_auth, reload_daemon_config, resolve_ui_dist,
    revert_listen_after_bind_failure, router, start_integration_watchers, validate_daemon_listen,
    AppState,
};
use crate::event_worker::start_event_runtime;
use crate::job_worker::start_job_runtime;
use crate::scheduler::spawn_scheduler;

#[derive(Debug, Parser)]
#[command(name = "bookclerkd", version, about = "Bookclerk background daemon")]
/// `bookclerkd` CLI flags (files dir, config path, listen overrides).
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

    /// Override HTTP listen address(es), comma-separated
    /// (`127.0.0.1:8787,[::1]:8787`).
    #[arg(long, env = "BOOKCLERK_DAEMON_LISTEN")]
    listen: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mut config = Config::load(args.bookclerk_files, args.config)?;
    if let Some(listen) = args.listen {
        config.daemon.listen = ListenAddrs::parse_list(&listen)
            .map_err(|err| anyhow::anyhow!("invalid --listen / BOOKCLERK_DAEMON_LISTEN: {err}"))?;
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
    bookclerk_plugin_host::retry_abandoned_acl_journals(&paths.files_dir);

    let bookclerk_plugin_host::HostControlPlane {
        config,
        database_registry,
        store: library_store,
        session: control_plane,
        enrolled,
        present,
        ..
    } = bookclerk_plugin_host::open_host_control_plane(&config).await?;
    tracing::info!(
        node_id = %control_plane.node.node_id,
        cluster_id = %control_plane.cluster_id,
        events_revision = control_plane.events.revision,
        output_revision = control_plane.output.revision,
        enrolled = enrolled.len(),
        present = present.len(),
        "enrolled control plane; core.events, core.output, and plugin deployments are database-authoritative"
    );
    // Guests are the deployment reconciler's: the registries start empty and
    // the first reconcile pass below brings every present deployment up
    // before the HTTP listeners bind.
    let integrations = bookclerk_integrations::IntegrationRegistry::new();
    let destinations = bookclerk_plugin_host::DestinationRegistry::default();
    let sources = bookclerk_source::SourceRegistry::new();
    let library = Arc::new(RwLock::new(library_store));
    let database_registry = Arc::new(RwLock::new(database_registry));
    validate_daemon_listen(&config)?;
    let operator_auth = {
        let lib = library.read().await.clone();
        build_operator_auth(&config, &lib).await?
    };

    let config = Arc::new(RwLock::new(config));
    let listen_reload = Arc::new(Notify::new());
    let process_shutdown = Arc::new(AtomicBool::new(false));
    let state = Arc::new(AppState {
        config: config.clone(),
        library: library.clone(),
        database_registry: database_registry.clone(),
        job_notify: Arc::new(Notify::new()),
        job_runtime: Arc::new(RwLock::new(())),
        work_lock: Mutex::new(()),
        discover_gate: Arc::new(Semaphore::new(1)),
        integrations: Arc::new(RwLock::new(integrations)),
        sources: Arc::new(RwLock::new(sources)),
        destinations: Arc::new(RwLock::new(destinations)),
        auth: Arc::new(RwLock::new(Arc::new(operator_auth))),
        reload_lock: Mutex::new(()),
        listen_reload: listen_reload.clone(),
        last_bound_listen: RwLock::new(None),
        tray: RwLock::new(None),
        tray_handoff: Mutex::new(None),
        deployment_runtime: std::sync::OnceLock::new(),
        library_swaps: tokio::sync::watch::Sender::new(0),
    });

    bookclerk_plugin_host::spawn_grant_watcher(paths.files_dir.clone());
    if let Err(err) = config_authority::reconcile_deployments(&state).await {
        tracing::warn!(error = %err, "initial plugin deployment reconcile failed");
    }
    start_integration_watchers(&state).await;
    if let Err(err) = crate::oidc::sync_plugin_oidc_clients(&state).await {
        tracing::warn!(error = %err, "failed to sync plugin OIDC clients");
    }
    start_job_runtime(state.clone()).await;
    start_event_runtime(state.clone());
    config_authority::spawn_config_reconciler(state.clone());
    config_authority::spawn_deployment_reconciler(state.clone());
    heartbeat::spawn_heartbeat(state.clone());
    // This install as a host node of the owner clusters it enrolled into.
    cluster_link::spawn_cluster_links(state.clone());
    placement::spawn_placement_scheduler(state.clone());
    maintenance::spawn_maintenance(state.clone());
    requirements_sweep::spawn_requirements_sweep(state.clone());
    spawn_scheduler(state.clone());
    spawn_config_reload_signals(state.clone());

    let ui_dist = resolve_ui_dist();
    let app = router(state.clone(), ui_dist);
    // The node API's TLS listener (acme / self_signed). Until it serves a
    // certificate the node API stays unmounted on every listener.
    let node_api_gate = {
        let cfg = config.read().await.clone();
        let files_dir = cfg.paths().files_dir.clone();
        // The listener serves the web app too: `public_origin` names it, and
        // a branch's owner signs in there.
        node_api::tls::start_with(&state, &cfg, &files_dir, Some(app.clone()))
            .await
            .gate
    };

    loop {
        if process_shutdown.load(Ordering::SeqCst) {
            break;
        }

        let listeners = loop {
            if process_shutdown.load(Ordering::SeqCst) {
                return Ok(());
            }

            let listen = config.read().await.daemon.listen.clone();
            match bind_listen_addrs(&listen).await {
                Ok(listeners) => {
                    *state.last_bound_listen.write().await = Some(listen);
                    break listeners;
                }
                Err(err) => {
                    let reverted = revert_listen_after_bind_failure(&state).await;
                    tracing::error!(
                        error = %err,
                        reverted,
                        "failed to bind any daemon.listen address; retrying in 5s or on config reload"
                    );
                    tokio::select! {
                        () = tokio::time::sleep(Duration::from_secs(5)) => {}
                        () = listen_reload.notified() => {
                            tracing::info!("daemon.listen changed during bind retry; re-reading address");
                        }
                    }
                }
            }
        };

        for listener in &listeners {
            if let Ok(addr) = listener.local_addr() {
                tracing::info!(%addr, "bookclerkd listening");
            }
        }

        // After bind so the tray's first browser open reaches a live listener.
        let cfg = config.read().await.clone();
        {
            let mut tray_slot = state.tray.write().await;
            match tray_slot.as_ref() {
                Some(handle) => {
                    let auth = state.auth_snapshot().await;
                    tray_companion::update_tray_after_reload(handle, &cfg, &auth);
                }
                None => {
                    let auth = state.auth_snapshot().await;
                    *tray_slot = tray_companion::maybe_spawn_tray(&cfg, &auth);
                }
            }
        }

        let mut set = tokio::task::JoinSet::new();
        for listener in listeners {
            // The node API's transport policy needs both ends of each
            // connection: the accepted socket's local address, which stays
            // loopback for a same-machine client on a wildcard bind.
            let app = app.clone().layer(axum::Extension(node_api_gate.clone()));
            let app = node_api::transport::ConnAddrsMakeService::new(app);
            let reload = listen_reload.clone();
            let shutdown_flag = process_shutdown.clone();
            set.spawn(async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(serve_shutdown(reload, shutdown_flag))
                    .await
            });
        }

        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(Ok(())) => {}
                Ok(Err(err)) => tracing::error!(error = %err, "HTTP listener exited with error"),
                Err(err) => tracing::error!(error = %err, "HTTP listener task panicked"),
            }
        }

        if process_shutdown.load(Ordering::SeqCst) {
            break;
        }
        tracing::info!("HTTP listeners shut down for rebind");
    }

    // Best-effort stop of integration watchers on process exit.
    state.integrations.read().await.stop_all().await;
    Ok(())
}

/// Completes when the process should exit or listeners must rebind after a config reload.
async fn serve_shutdown(listen_reload: Arc<Notify>, process_shutdown: Arc<AtomicBool>) {
    let process = shutdown_signal();
    tokio::select! {
        () = process => {
            process_shutdown.store(true, Ordering::SeqCst);
        }
        () = listen_reload.notified() => {}
    }
}

/// Installs a SIGHUP handler that reloads daemon config (no-op on non-Unix).
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

/// Waits for Ctrl+C or SIGTERM before tearing down HTTP listeners.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler");
        tokio::select! {
            () = ctrl_c => {},
            _ = terminate.recv() => {},
        }
    }

    #[cfg(not(unix))]
    {
        ctrl_c.await;
    }
}
