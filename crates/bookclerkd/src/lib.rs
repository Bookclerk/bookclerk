//! `bookclerkd` — long-running scan / acquire daemon with HTTP control plane.
//!
//! The library holds the whole host. The `bookclerkd` binary runs it behind
//! the HTTP listeners ([`run`]); the desktop window runs the same host with
//! no listener and hands it requests in-process ([`embedded`]).

// Axum `Response` as handler `Err` exceeds Clippy's 128-byte `result_large_err` cap.
#![allow(clippy::result_large_err)]
// Pre-existing sites, grandfathered until fixed: https://github.com/Bookclerk/bookclerk/issues/284
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::expect_used
)]

mod admin_controls;
mod api;
mod auth;
mod branch_requests;
mod cache_jobs;
mod cache_policy;
mod catalog_lookup;
mod clerk_link;
mod clerk_token;
mod collections;
mod config_authority;
mod covers;
mod cron_worker;
mod cross_origin;
mod csrf;
mod destination_sync;
mod diagnostics_lookup;
mod discover_events;
mod discover_jobs;
mod event_worker;
mod heartbeat;
mod http_error;
mod ingress;
mod job_handler;
mod job_worker;
mod jobs;
mod library_index;
mod library_link;
mod library_overrides;
mod library_works;
mod live;
mod maintenance;
mod master_key_recovery;
mod media_capabilities;
mod node_api;
mod node_queue;
mod oidc;
mod oidc_rp;
mod oidc_verify;
mod openapi;
#[cfg(test)]
mod openapi_contract;
mod origin;
mod passkeys;
mod patron_scopes;
mod placement;
mod playback;
mod plugin_installs;
mod profile;
/// Builds the daemon source / integration registry for [`AppState`].
mod registry;
mod request_queue_job;
mod requirements_sweep;
mod restart;
mod scheduler;
mod search_index;
#[cfg(test)]
mod startup_tests;
mod totp;
mod tray_companion;
mod uploads;
mod work_tags;

pub mod embedded;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bookclerk_config::{
    init_tracing_with, Config, ListenAddrs, LogFormat, LoggingHandle, ProcessRole, TracingOptions,
};
use tokio::sync::{Mutex, Notify, RwLock, Semaphore};

use crate::api::{
    bind_listen_addrs, build_operator_auth, resolve_ui_dist, revert_listen_after_bind_failure,
    router, start_integration_watchers, validate_daemon_listen, AppState,
};
// Only the Unix SIGHUP handler reloads config from a signal.
#[cfg(unix)]
use crate::api::reload_daemon_config;
use crate::event_worker::start_event_runtime;
use crate::job_worker::start_job_runtime;
use crate::scheduler::spawn_scheduler;

#[derive(Debug, clap::Parser)]
#[command(name = "bookclerkd", version, about = "Bookclerk background daemon")]
/// `bookclerkd` CLI flags (files dir, config path, listen overrides).
pub struct Args {
    /// Bookclerk files directory.
    #[arg(
        long = "bookclerk-files",
        visible_alias = "bookclerkFiles",
        env = "BOOKCLERK_FILES_DIR"
    )]
    pub bookclerk_files: Option<PathBuf>,

    /// Path to config.toml.
    #[arg(long, env = "BOOKCLERK_CONFIG")]
    pub config: Option<PathBuf>,

    /// Override HTTP listen address(es), comma-separated
    /// (`127.0.0.1:8787,[::1]:8787`).
    #[arg(long, env = "BOOKCLERK_DAEMON_LISTEN")]
    pub listen: Option<String>,

    /// Print the HTTP API's OpenAPI document (JSON) to stdout and exit,
    /// without reading any configuration. `3.1` (the default) is the
    /// document the daemon serves at `/api/openapi.json`; `3.0` is the same
    /// API restated for tools that only read OpenAPI 3.0.
    #[arg(long, value_name = "VERSION", num_args = 0..=1, default_missing_value = "3.1")]
    pub print_openapi: Option<OpenApiVersion>,
}

/// Which OpenAPI version `--print-openapi` writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OpenApiVersion {
    /// OpenAPI 3.1, the document the daemon serves at `/api/openapi.json`.
    #[value(name = "3.1")]
    V31,
    /// OpenAPI 3.0.3, served at `/api/openapi-3.0.json`.
    #[value(name = "3.0")]
    V30,
}

/// How many more times the first start after a shared branch join tries the
/// library's database before undoing the join.
const SHARED_BRANCH_JOIN_OPEN_RETRIES: u32 = 3;

/// Runs the daemon behind its HTTP listeners until the process is asked to
/// stop (Ctrl+C, SIGTERM) or to restart onto new bootstrap files.
///
/// # Arguments
///
/// * `args` - The parsed command line.
///
/// # Errors
///
/// Returns an error when the configuration cannot be loaded, the host cannot
/// start (the control plane, an interrupted shared branch join, operator auth),
/// `daemon.listen` is invalid, or a restart cannot re-exec the binary.
pub async fn run(args: Args) -> anyhow::Result<()> {
    if let Some(version) = args.print_openapi {
        let text = match version {
            OpenApiVersion::V31 => openapi::document_json()?,
            OpenApiVersion::V30 => openapi::document_v30_json()?,
        };
        print!("{text}");
        return Ok(());
    }
    let mut config = Config::load(args.bookclerk_files, args.config)?;
    if let Some(listen) = args.listen {
        config.daemon.listen = ListenAddrs::parse_list(&listen)
            .map_err(|err| anyhow::anyhow!("invalid --listen / BOOKCLERK_DAEMON_LISTEN: {err}"))?;
    }

    init_logging(&config, "bookclerkd", ProcessRole::Daemon);
    serve_listeners(start_host(config, HostMode::Listening).await?).await
}

/// Installs the process-wide tracing subscriber for `config` and logs where
/// output goes, then prepares the confined codec pool.
///
/// # Arguments
///
/// * `config` - The loaded configuration (log format, diagnostics, media).
/// * `syslog_identifier` - The name the OS log facility records
///   (`bookclerkd`, or the desktop window's binary name).
///
/// The logging handle is kept in [`LOGGING`] for the rest of the process,
/// so stderr logging keeps running after this returns. A second call keeps
/// the first handle; its own subscriber is not installed either.
fn init_logging(config: &Config, syslog_identifier: &str, role: ProcessRole) {
    let log_format = if config.daemon.json_logs {
        LogFormat::Json
    } else {
        LogFormat::Text
    };
    let logging = init_tracing_with(TracingOptions {
        format: log_format,
        default_level: "bookclerk=info,warn".into(),
        syslog_identifier: syslog_identifier.into(),
        diagnostics: config.diagnostics.clone(),
        version: env!("CARGO_PKG_VERSION").into(),
        enable_journald: true,
        role,
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
    start_usage_checkins(config, &logging.diagnostics, role);
    keep_logging(logging);
}

/// How often the check-in thread looks whether a weekly check-in is due.
const CHECKIN_POLL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// How often the check-in thread reads the live usage-sharing switch, so a
/// check-in follows soon after the switch is turned on in Settings.
const SHARING_POLL: std::time::Duration = std::time::Duration::from_secs(60);

/// Points the diagnostics handle at the files directory (so accepted
/// reports join the install's report history) and, unless this is a demo
/// copy, starts the thread that sends the weekly check-in while
/// `diagnostics.share_usage` is on (see `docs/server/diagnostics.md`).
///
/// The thread reads the live setting on every pass, so turning usage
/// sharing on or off in Settings takes effect without a restart. The
/// install's random ID is loaded (or created) the first time the setting is
/// on, never before.
///
/// # Arguments
///
/// * `config` - The loaded configuration (files directory, diagnostics).
/// * `diagnostics` - The process's diagnostics handle.
/// * `role` - What kind of process this is, reported as the check-in's kind.
fn start_usage_checkins(
    config: &Config,
    diagnostics: &bookclerk_config::DiagnosticsHandle,
    role: ProcessRole,
) {
    let files_dir = config.paths().files_dir.clone();
    diagnostics.set_files_dir(&files_dir);
    if config.diagnostics.demo {
        return;
    }
    if config.diagnostics.share_usage {
        tracing::info!(
            "diagnostics.share_usage=true — a weekly anonymous check-in goes to the collector"
        );
    }
    let settings = config.diagnostics.clone();
    let handle = diagnostics.clone();
    let spawned = std::thread::Builder::new()
        .name("bookclerk-checkin".into())
        .spawn(move || {
            let mut checkin: Option<bookclerk_config::CheckIn> = None;
            let mut was_on = false;
            let mut last_pass: Option<std::time::Instant> = None;
            loop {
                let on = handle.share_usage();
                let due = last_pass.is_none_or(|at| at.elapsed() >= CHECKIN_POLL);
                // A pass right away when the switch turns on, then every poll.
                if on && (!was_on || due) {
                    checkin_pass(&files_dir, &settings, &handle, role, &mut checkin);
                    last_pass = Some(std::time::Instant::now());
                }
                was_on = on;
                std::thread::sleep(SHARING_POLL);
            }
        });
    if let Err(err) = spawned {
        tracing::warn!(error = %err, "usage check-ins off: the thread could not start");
    }
}

/// One pass of the check-in thread while usage sharing is on: loads the
/// install ID on first use, then sends the check-in when one is due.
///
/// # Arguments
///
/// * `files_dir` - The Bookclerk files directory.
/// * `settings` - Diagnostics settings (collector URL).
/// * `diagnostics` - The process's diagnostics handle, which gets the ID.
/// * `role` - Reported as the check-in's kind.
/// * `checkin` - The check-in body, built on the first pass that has an ID.
fn checkin_pass(
    files_dir: &std::path::Path,
    settings: &bookclerk_config::DiagnosticsConfig,
    diagnostics: &bookclerk_config::DiagnosticsHandle,
    role: ProcessRole,
    checkin: &mut Option<bookclerk_config::CheckIn>,
) {
    if checkin.is_none() {
        let id = match bookclerk_config::InstallId::load_or_create(files_dir) {
            Ok(id) => id,
            Err(err) => {
                tracing::warn!(error = %err, "usage check-in skipped: the install ID could not be read or written");
                return;
            }
        };
        diagnostics.set_install_id(&id);
        *checkin = Some(bookclerk_config::CheckIn {
            install_id: id.as_str().to_string(),
            version: env!("CARGO_PKG_VERSION").into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            kind: role.as_str().into(),
        });
    }
    let Some(body) = checkin.as_ref() else {
        return;
    };
    let now = unix_now_ms();
    if !bookclerk_config::checkin_due(files_dir, now) {
        return;
    }
    match bookclerk_config::send_checkin(settings, body) {
        Ok(()) => {
            if let Err(err) = bookclerk_config::record_checkin(files_dir, now) {
                tracing::warn!(error = %err, "usage check-in sent but not recorded");
            }
        }
        Err(err) => tracing::debug!(error = %err, "usage check-in not sent; retrying later"),
    }
}

/// Wall-clock Unix milliseconds, or 0 before the epoch.
fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The process's logging handle, kept until the process exits: it owns the
/// non-blocking stderr writer, which stops when the handle is dropped.
/// Logging is process-wide, so the handle is too.
static LOGGING: OnceLock<LoggingHandle> = OnceLock::new();

/// Keeps `logging` in [`LOGGING`] for the rest of the process.
///
/// # Arguments
///
/// * `logging` - The handle [`init_logging`] just built. When a handle is
///   already kept, this one is dropped: its subscriber was not installed,
///   so nothing writes through it.
///
/// # Returns
///
/// Whether `logging` was kept (`false` when an earlier handle already was).
fn keep_logging(logging: LoggingHandle) -> bool {
    LOGGING.set(logging).is_ok()
}

/// Whether the host serves HTTP listeners or runs embedded in a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostMode {
    /// `bookclerkd`: `daemon.listen` is validated and bound.
    Listening,
    /// The desktop window: no listener, so `daemon.listen` is not checked.
    Embedded,
}

/// Sets where this host writes new keys, and moves the desktop app's key
/// into the OS keychain.
///
/// `[auth].key_store` wins when set. Otherwise the desktop window defaults
/// to the OS keychain and `bookclerkd` to `master.key`. On the desktop
/// default, a key still in `master.key` is moved into the keychain; if the
/// keychain can't take it, the key stays in the file and the start goes on.
fn apply_key_store(config: &Config, mode: HostMode) {
    use bookclerk_config::KeyStoreKind;
    use bookclerk_library::MasterKeyFormat;
    let default = match mode {
        HostMode::Listening => KeyStoreKind::File,
        HostMode::Embedded => KeyStoreKind::Keychain,
    };
    if let Ok(mut slot) = KEY_STORE_HOST_DEFAULT.write() {
        *slot = default;
    }
    let preference = refresh_key_store_preference(config);
    if preference.kind != KeyStoreKind::Keychain || preference.required {
        return;
    }
    let files_dir = &config.paths().files_dir;
    match bookclerk_library::inspect_master_key(files_dir) {
        Ok(Some(MasterKeyFormat::Raw | MasterKeyFormat::Wrapped)) => {}
        Ok(Some(MasterKeyFormat::Keychain) | None) => return,
        Err(err) => {
            tracing::warn!("not moving master.key into the OS keychain: {err}");
            return;
        }
    }
    let password = config.auth_password();
    match bookclerk_library::move_master_key(files_dir, KeyStoreKind::Keychain, password.as_deref())
    {
        Ok(_) => tracing::info!(
            "moved the data-encryption key into the OS keychain; save a recovery copy with \
             `bookclerk config master-key export --out <file>`"
        ),
        Err(err) => tracing::warn!(
            "the data-encryption key stays in master.key: the OS keychain could not take it ({err})"
        ),
    }
}

/// Where this host puts new keys when `[auth].key_store` is unset: the
/// file for `bookclerkd`, the OS keychain for the desktop window. Set by
/// [`apply_key_store`] at start.
static KEY_STORE_HOST_DEFAULT: std::sync::RwLock<bookclerk_config::KeyStoreKind> =
    std::sync::RwLock::new(bookclerk_config::KeyStoreKind::File);

/// Sets where new keys go from `config`'s `[auth].key_store` and this
/// host's default, as at start; a config reload calls it so a changed
/// setting applies to the next key placed (a shared-branch join).
///
/// # Returns
///
/// The preference now in effect.
pub(crate) fn refresh_key_store_preference(
    config: &Config,
) -> bookclerk_library::KeyStorePreference {
    let default = KEY_STORE_HOST_DEFAULT
        .read()
        .map(|slot| *slot)
        .unwrap_or(bookclerk_config::KeyStoreKind::File);
    let preference =
        bookclerk_library::KeyStorePreference::from_config(config.auth.key_store, default);
    bookclerk_library::set_key_store_preference(preference);
    preference
}

/// What [`start_host`] hands back: the running host and its router.
struct HostParts {
    /// Shared daemon state the background runtimes already hold.
    state: Arc<AppState>,
    /// The web app and API router (without the node API gate extension).
    app: axum::Router,
    /// Live configuration, also held by `state`.
    config: Arc<RwLock<Config>>,
    /// Notified when `daemon.listen` changes and listeners must rebind.
    listen_reload: Arc<Notify>,
    /// Set once the process is shutting down.
    process_shutdown: Arc<AtomicBool>,
    /// The library's ingress host, in [`HostMode::Listening`] only: the
    /// listeners hand it their router for tunnel requests.
    ingress: Option<Arc<ingress::DaemonIngress>>,
}

/// Opens the control plane and starts every background runtime (deployment
/// reconcile, jobs, events, heartbeat, library links, scheduler), then builds
/// the router. Binds nothing: the caller decides how requests reach it.
///
/// # Arguments
///
/// * `config` - The loaded configuration; logging is already installed.
/// * `mode` - Whether `daemon.listen` will be bound (and so must be valid).
///
/// # Errors
///
/// Returns an error when the files directory cannot be prepared, a shared branch join
/// was interrupted or had to be undone, the control plane does not open,
/// `daemon.listen` is invalid in [`HostMode::Listening`], or operator auth
/// cannot be built.
async fn start_host(config: Config, mode: HostMode) -> anyhow::Result<HostParts> {
    let paths = config.paths().clone();
    paths.ensure_dirs()?;
    // Before the node identity, master key or database is opened: a
    // half-applied shared branch join would pair a database with the wrong node
    // identity or key.
    if let Some(problem) =
        bookclerk_node_client::interrupted_shared_branch_join(&paths.files_dir, &paths.config_file)
    {
        anyhow::bail!(problem);
    }
    // A shared branch join applied before this start is confirmed once the
    // library's database opens, and undone if it does not; one whose undoing
    // was cut short is finished first.
    let mut pending_join =
        bookclerk_node_client::pending_shared_branch_join(&paths.files_dir, &paths.config_file);
    if let Some(join) = pending_join.take_if(|join| join.reverting()) {
        return match join.revert() {
            Ok(()) => Err(anyhow::anyhow!(
                "finished undoing an earlier shared branch join: this server's own settings, node identity and key are back; start it again to run on them"
            )),
            Err(failed) => Err(anyhow::anyhow!(
                "could not finish undoing an earlier shared branch join ({failed}); fix that and start again"
            )),
        };
    }
    bookclerk_plugin_host::retry_abandoned_acl_journals(&paths.files_dir);
    apply_key_store(&config, mode);

    let opened = match pending_join {
        None => bookclerk_plugin_host::open_host_control_plane(&config).await?,
        Some(join) => {
            // A database that is only slow to come up (a restarting
            // PostgreSQL, a DNS or network blip) gets a few tries before the
            // join is undone.
            let mut tries = 0u32;
            let opened = loop {
                match bookclerk_plugin_host::open_host_control_plane(&config).await {
                    Ok(opened) => break Ok(opened),
                    Err(err) if tries < SHARED_BRANCH_JOIN_OPEN_RETRIES => {
                        tries += 1;
                        tracing::warn!(
                            attempt = tries,
                            error = %err,
                            "the library's database did not open after joining as a shared branch; trying again"
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(5 * u64::from(tries)))
                            .await;
                    }
                    Err(err) => break Err(err),
                }
            };
            match opened {
                Ok(opened) => {
                    join.confirm();
                    tracing::info!(
                        "the library's database opened; the shared branch join is complete"
                    );
                    opened
                }
                Err(err) => {
                    tracing::error!(error = %err, "the library's database did not open after joining as a shared branch; undoing the join");
                    return Err(match join.revert() {
                        Ok(()) => anyhow::anyhow!(
                            "the library's database did not open after joining as a shared branch (see the log), so the join was undone: this server's own settings, node identity and key are back. Start it again to run on them, and have it removed in the library's Settings, Shared branches"
                        ),
                        Err(failed) if failed.started => anyhow::anyhow!(
                            "the library's database did not open after joining as a shared branch (see the log), and undoing the join stopped part way ({failed}); the next start finishes undoing it"
                        ),
                        Err(failed) => anyhow::anyhow!(
                            "the library's database did not open after joining as a shared branch (see the log), and the join could not be undone ({failed}); if it is left as it is, the next start tries the library's database again"
                        ),
                    });
                }
            }
        }
    };
    assemble_host(opened, mode).await
}

/// Starts every background runtime (deployment reconcile, jobs, events,
/// heartbeat, library links, scheduler) on an opened control plane, then
/// builds the router. Binds nothing.
///
/// # Arguments
///
/// * `opened` - The control plane [`start_host`] opened (or a test's, on an
///   in-process database).
/// * `mode` - Whether `daemon.listen` will be bound (and so must be valid).
///
/// # Errors
///
/// Returns an error when `daemon.listen` is invalid in
/// [`HostMode::Listening`] or operator auth cannot be built.
async fn assemble_host(
    opened: bookclerk_plugin_host::HostControlPlane,
    mode: HostMode,
) -> anyhow::Result<HostParts> {
    let bookclerk_plugin_host::HostControlPlane {
        config,
        database_registry,
        store: library_store,
        session: control_plane,
        enrolled,
        present,
        ..
    } = opened;
    let paths = config.paths().clone();
    tracing::info!(
        node_id = %control_plane.node.node_id,
        library_id = %control_plane.library_id,
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
    if mode == HostMode::Listening {
        validate_daemon_listen(&config)?;
    }
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
        capacity: Arc::new(crate::library_link::capacity::CapacityShares::new()),
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
        node_api_tls_bound: std::sync::OnceLock::new(),
        in_window: std::sync::OnceLock::new(),
        library_swaps: tokio::sync::watch::Sender::new(0),
        covers: Arc::new(open_cover_service(&paths.cache_dir)),
    });

    // Only a listening daemon serves a public address: the ingress host is
    // set before the first reconcile so an active ingress starts with it.
    let ingress = (mode == HostMode::Listening).then(|| {
        let ingress = Arc::new(ingress::DaemonIngress::new(
            &state,
            tokio::runtime::Handle::current(),
        ));
        state
            .deployment_runtime()
            .set_ingress_host(Arc::clone(&ingress) as Arc<dyn bookclerk_plugin_host::IngressHost>);
        ingress
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
    // This install as a branch node of the libraries it enrolled into.
    library_link::spawn_library_links(state.clone());
    // The libraries this install holds a library card for (as a clerk).
    clerk_link::spawn_clerk_links(state.clone());
    placement::spawn_placement_scheduler(state.clone());
    maintenance::spawn_maintenance(state.clone());
    uploads::spawn_prune(state.clone());
    cron_worker::spawn_cron_worker(state.clone());
    requirements_sweep::spawn_requirements_sweep(state.clone());
    spawn_scheduler(state.clone());
    spawn_config_reload_signals(state.clone());

    let ui_dist = resolve_ui_dist();
    let app = router(state.clone(), ui_dist);
    Ok(HostParts {
        state,
        app,
        config,
        listen_reload,
        process_shutdown,
        ingress,
    })
}

/// Binds `daemon.listen` and serves the host's router on every address until
/// the process shuts down, rebinding when the address changes and re-executing
/// the binary when a restart was requested.
///
/// # Arguments
///
/// * `host` - The running host from [`start_host`].
///
/// # Errors
///
/// Returns an error when a requested restart cannot re-exec the binary.
async fn serve_listeners(host: HostParts) -> anyhow::Result<()> {
    let HostParts {
        state,
        app,
        config,
        listen_reload,
        process_shutdown,
        ingress,
    } = host;
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
    // Tunnel requests reach the same router, gate included.
    if let Some(ingress) = &ingress {
        ingress.set_router(app.clone().layer(axum::Extension(node_api_gate.clone())));
    }

    loop {
        if process_shutdown.load(Ordering::SeqCst) {
            break;
        }

        let listeners = loop {
            if process_shutdown.load(Ordering::SeqCst) {
                if restart::is_requested() {
                    restart::reexec()?;
                }
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
    if restart::is_requested() {
        // New bootstrap files (a shared branch join): start again on them.
        restart::reexec()?;
    }
    Ok(())
}

/// The cover service caching under `<cache>/covers`, or a detached one that
/// serves no covers (logged) when that directory cannot be prepared.
fn open_cover_service(cache_dir: &std::path::Path) -> covers::CoverService {
    covers::CoverService::open(cache_dir.join("covers"), covers::Limits::default()).unwrap_or_else(
        |err| {
            tracing::error!(error = %err, "cover cache unavailable; covers will not be served");
            covers::CoverService::detached()
        },
    )
}

/// Completes when the process should exit or listeners must rebind after a config reload.
async fn serve_shutdown(listen_reload: Arc<Notify>, process_shutdown: Arc<AtomicBool>) {
    let process = shutdown_signal();
    tokio::select! {
        () = process => {
            process_shutdown.store(true, Ordering::SeqCst);
        }
        () = restart::requested() => {
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

#[cfg(test)]
mod logging_tests {
    use super::{init_logging, Config, LoggingHandle, LOGGING};

    /// Where the kept handle lives, to tell one handle from another.
    fn kept() -> Option<*const LoggingHandle> {
        LOGGING.get().map(std::ptr::from_ref)
    }

    #[test]
    fn logging_outlives_its_setup_and_a_second_setup_keeps_the_first() {
        let files = tempfile::tempdir().unwrap();
        let config = Config::load(Some(files.path().to_path_buf()), None).unwrap();
        init_logging(&config, "bookclerkd-test", super::ProcessRole::Daemon);
        let first = kept();
        assert!(first.is_some(), "the handle must outlive init_logging");
        init_logging(&config, "bookclerkd-test", super::ProcessRole::Daemon);
        assert_eq!(kept(), first, "a second setup keeps the first handle");
    }
}

#[cfg(test)]
mod checkin_tests {
    use super::{checkin_pass, ProcessRole};
    use bookclerk_config::{DiagnosticsConfig, DiagnosticsHandle};
    use std::io::{Read, Write};

    /// A collector that accepts one request and answers 200 with no body.
    fn accept_once() -> (String, std::thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut raw = Vec::new();
            let mut buf = [0_u8; 4096];
            loop {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw);
                if let Some(end) = text.find("\r\n\r\n") {
                    let len = text[..end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if raw.len() >= end + 4 + len {
                        break;
                    }
                }
            }
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            String::from_utf8_lossy(&raw).into_owned()
        });
        (url, server)
    }

    fn handle(collector_url: &str) -> (DiagnosticsConfig, DiagnosticsHandle) {
        let settings = DiagnosticsConfig {
            collector_url: collector_url.into(),
            share_usage: true,
            ..DiagnosticsConfig::default()
        };
        let handle = DiagnosticsHandle::new(settings.clone(), "0.0.0-test", ProcessRole::Daemon);
        handle.set_sharing(false, true);
        (settings, handle)
    }

    #[test]
    fn the_first_pass_creates_the_id_and_sends_once() {
        let dir = tempfile::tempdir().unwrap();
        let (url, server) = accept_once();
        let (settings, diag) = handle(&url);
        let mut checkin = None;
        checkin_pass(
            dir.path(),
            &settings,
            &diag,
            ProcessRole::Daemon,
            &mut checkin,
        );
        let request = server.join().unwrap();
        let body = checkin.as_ref().expect("the check-in body is built");
        assert!(request.starts_with("POST /checkin "), "{request}");
        assert!(request.contains(&body.install_id), "{request}");
        assert_eq!(body.kind, ProcessRole::Daemon.as_str());
        assert!(dir.path().join("install-id").is_file());
        assert!(
            !bookclerk_config::checkin_due(dir.path(), super::unix_now_ms()),
            "an accepted check-in is recorded"
        );
        // Not due again: the second pass sends nothing (no collector listens).
        checkin_pass(
            dir.path(),
            &settings,
            &diag,
            ProcessRole::Daemon,
            &mut checkin,
        );
    }

    #[test]
    fn an_unwritable_files_dir_skips_the_pass() {
        let dir = tempfile::tempdir().unwrap();
        let not_a_dir = dir.path().join("file");
        std::fs::write(&not_a_dir, b"x").unwrap();
        let (settings, diag) = handle("http://127.0.0.1:9");
        let mut checkin = None;
        checkin_pass(
            &not_a_dir,
            &settings,
            &diag,
            ProcessRole::Daemon,
            &mut checkin,
        );
        assert!(checkin.is_none());
    }

    #[test]
    fn a_refused_send_is_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        let (settings, diag) = handle(&url);
        let mut checkin = None;
        checkin_pass(
            dir.path(),
            &settings,
            &diag,
            ProcessRole::Daemon,
            &mut checkin,
        );
        assert!(checkin.is_some());
        assert!(bookclerk_config::checkin_due(
            dir.path(),
            super::unix_now_ms()
        ));
    }
}

#[cfg(test)]
mod key_store_tests {
    use super::{apply_key_store, Config, HostMode};
    use bookclerk_config::KeyStoreKind;
    use bookclerk_keystore::{KeystoreErrorKind, MemoryVault, OsKeychain};
    use bookclerk_library::{inspect_master_key, KeyStorePreference, MasterKeyFormat};
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

    /// Serializes tests that swap the process-wide keychain and preference.
    static LOCK: Mutex<()> = Mutex::new(());

    /// An in-memory keychain for one test; the OS keychain and the file
    /// default come back on drop.
    struct Scope {
        /// Held for the scope's lifetime.
        _guard: MutexGuard<'static, ()>,
        /// The in-memory keychain the scope installed.
        vault: Arc<MemoryVault>,
    }

    impl Scope {
        fn new() -> Self {
            let guard = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
            let vault = Arc::new(MemoryVault::new());
            bookclerk_library::set_keychain_vault(vault.clone());
            Self {
                _guard: guard,
                vault,
            }
        }
    }

    impl Drop for Scope {
        fn drop(&mut self) {
            bookclerk_library::set_keychain_vault(Arc::new(OsKeychain::new()));
            bookclerk_library::set_key_store_preference(KeyStorePreference::from_config(
                None,
                KeyStoreKind::File,
            ));
        }
    }

    /// A config for a fresh files directory holding a raw key.
    fn config_with_raw_key(files: &std::path::Path, key_store: Option<KeyStoreKind>) -> Config {
        bookclerk_library::set_key_store_preference(KeyStorePreference::from_config(
            None,
            KeyStoreKind::File,
        ));
        bookclerk_library::install_master_key(files, &[4u8; 32], None).unwrap();
        let mut config = Config::load(Some(files.to_path_buf()), None).unwrap();
        config.auth.password = None;
        config.auth.key_store = key_store;
        config
    }

    #[test]
    fn the_desktop_default_moves_a_file_key_into_the_keychain() {
        let scope = Scope::new();
        let files = tempfile::tempdir().unwrap();
        let config = config_with_raw_key(files.path(), None);
        apply_key_store(&config, HostMode::Embedded);
        let preference = bookclerk_library::key_store_preference();
        assert_eq!(preference.kind, KeyStoreKind::Keychain);
        assert!(!preference.required);
        assert_eq!(
            inspect_master_key(files.path()).unwrap(),
            Some(MasterKeyFormat::Keychain)
        );
        assert_eq!(scope.vault.len(), 1);
        // A second start finds the pointer and leaves it alone.
        apply_key_store(&config, HostMode::Embedded);
        assert_eq!(scope.vault.len(), 1);

        // A reload that sets the store makes it required; unsetting it
        // returns to this host's default.
        let mut reloaded = config.clone();
        reloaded.auth.key_store = Some(KeyStoreKind::File);
        let preference = super::refresh_key_store_preference(&reloaded);
        assert_eq!(preference.kind, KeyStoreKind::File);
        assert!(preference.required);
        let preference = super::refresh_key_store_preference(&config);
        assert_eq!(preference.kind, KeyStoreKind::Keychain);
        assert!(!preference.required);
    }

    #[test]
    fn the_desktop_keeps_the_file_when_the_keychain_refuses() {
        let scope = Scope::new();
        let files = tempfile::tempdir().unwrap();
        let config = config_with_raw_key(files.path(), None);
        scope.vault.fail_with(Some(KeystoreErrorKind::Unavailable));
        apply_key_store(&config, HostMode::Embedded);
        assert_eq!(
            inspect_master_key(files.path()).unwrap(),
            Some(MasterKeyFormat::Raw)
        );
    }

    #[test]
    fn the_daemon_and_a_configured_store_never_move_the_key() {
        let scope = Scope::new();
        let files = tempfile::tempdir().unwrap();
        let config = config_with_raw_key(files.path(), None);
        apply_key_store(&config, HostMode::Listening);
        assert_eq!(
            bookclerk_library::key_store_preference().kind,
            KeyStoreKind::File
        );

        let configured = config_with_raw_key(files.path(), Some(KeyStoreKind::Keychain));
        apply_key_store(&configured, HostMode::Embedded);
        let preference = bookclerk_library::key_store_preference();
        assert_eq!(preference.kind, KeyStoreKind::Keychain);
        assert!(preference.required);
        assert_eq!(
            inspect_master_key(files.path()).unwrap(),
            Some(MasterKeyFormat::Raw)
        );
        assert!(scope.vault.is_empty());

        // No key yet: nothing to move.
        let empty = tempfile::tempdir().unwrap();
        let mut fresh = Config::load(Some(empty.path().to_path_buf()), None).unwrap();
        fresh.auth.key_store = None;
        apply_key_store(&fresh, HostMode::Embedded);
        assert_eq!(inspect_master_key(empty.path()).unwrap(), None);

        // An unreadable master.key is reported and left alone.
        let broken = tempfile::tempdir().unwrap();
        std::fs::write(broken.path().join("master.key"), b"nope").unwrap();
        let mut odd = Config::load(Some(broken.path().to_path_buf()), None).unwrap();
        odd.auth.key_store = None;
        apply_key_store(&odd, HostMode::Embedded);
        assert!(scope.vault.is_empty());
    }
}
