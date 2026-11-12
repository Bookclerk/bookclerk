//! HTTP control plane for `bookclerkd` (operator API + static GUI).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::Request;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Json;
use axum::Router;
use bookclerk_acquire::sidecar_key;
use bookclerk_config::{Config, ListenAddrs};
use bookclerk_integrations::{portal_spa_routes, IntegrationRegistry, PortalState};
use bookclerk_library::control_plane::ScanRequester;
use bookclerk_library::{
    hash_token, AcquireStatus, BookRecord, ClaimTicketRecord, JobRecord, JobTrigger, LibraryError,
    LibraryStore, NewTitleRequest, NewTitleRequestSource, QueueWisher, RequestStatus,
    TitleRequestRecord,
};
use bookclerk_plugin_host::{
    consent_request, consent_summary, cores_to_percent, effective_cpu_cores, format_cpu_cores,
    grant_covers, host_cpu_cores_max, pending_structural, percent_to_cores, require_grant,
    validate_approved_grant, DatabaseRegistry, DestinationRegistry, GrantedEventConsumer,
    PluginGrant, PluginGrantStore, PluginRuntimeKind, WorkerdLimits, KNOWN_HOST_BINDINGS,
    PLUGIN_JAIL_CPU_CORES_DEFAULT, PLUGIN_JAIL_CPU_RATE_DEFAULT,
    PLUGIN_JAIL_EXTRA_PROCESSES_DEFAULT, PLUGIN_JAIL_EXTRA_PROCESSES_MAX,
    PLUGIN_JAIL_MEMORY_MIB_DEFAULT, PLUGIN_JAIL_MEMORY_MIB_MAX, PLUGIN_STATE_BUDGET_MIB_DEFAULT,
    PLUGIN_STATE_BUDGET_MIB_MAX,
};
use bookclerk_search::SearchEngine;
use bookclerk_source::SourceRegistry;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify, RwLock, Semaphore};
use tokio::time::timeout;
use tower_http::classify::ServerErrorsFailureClass;
use tower_http::normalize_path::NormalizePathLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;
use tracing::Level;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::{self, OperatorAuthState};
use crate::http_error;
use crate::jobs::{
    enqueue_acquire, enqueue_integration_scan, enqueue_listen_sync, enqueue_scan, AdmitJob,
};
use crate::openapi::{Access, DocumentedGroup, RouteGroup};

/// Shared daemon state.
pub struct AppState {
    /// Live daemon config swapped under `reload_lock`; handlers clone then drop the read lock.
    pub config: Arc<RwLock<Config>>,
    /// Shared library store; clone the handle and drop the lock before awaiting DB I/O.
    pub library: Arc<RwLock<LibraryStore>>,
    /// Loaded database plugin guests; replaced when `[database].plugin` reloads.
    pub database_registry: Arc<RwLock<DatabaseRegistry>>,
    /// Wakes the durable job worker when a new row is admitted.
    pub job_notify: Arc<Notify>,
    /// Shared barrier covering admission, claim, and handler execution.
    ///
    /// Workers and admit hold a read permit. A database plugin swap takes the
    /// write permit so it cannot observe `jobs_in_flight == 0` while a worker
    /// is about to claim.
    pub job_runtime: Arc<RwLock<()>>,
    /// Serialize scan/acquire work so jobs do not thrash the same accounts.
    ///
    /// This is about store rate limits and the shared `StorageIndex`, not about
    /// CPU: codec concurrency is bounded by the media worker pool instead.
    /// Letting independent books acquire in parallel needs a concurrency-safe
    /// storage index first.
    pub work_lock: Mutex<()>,
    /// Cap concurrent discover/embed work so ONNX load + inference cannot saturate
    /// the Tokio blocking pool (and starve accept / `/health`) under page refresh.
    pub discover_gate: Arc<Semaphore>,
    /// Loaded integration guests used by watchers, the portal, and listening sync.
    pub integrations: Arc<RwLock<IntegrationRegistry>>,
    /// Loaded storefront source guests used by scan, acquire, and discover.
    pub sources: Arc<RwLock<SourceRegistry>>,
    /// Loaded output destinations that receive acquire artifacts.
    pub destinations: Arc<RwLock<DestinationRegistry>>,
    /// Operator auth runtime (enabled flag + token + sessions). Replaced on reload.
    ///
    /// Readers clone the inner [`Arc`] and drop the `RwLock` immediately so login /
    /// authorize paths never hold the writer out across handler `.await`s.
    pub auth: Arc<RwLock<Arc<OperatorAuthState>>>,
    /// Serializes config reload so candidate runtime swaps are not interleaved.
    pub reload_lock: Mutex<()>,
    /// Wakes the HTTP server to rebind when `daemon.listen` changes on config reload.
    pub listen_reload: Arc<Notify>,
    /// Last successfully bound listen set; used to roll back a failed rebind.
    pub last_bound_listen: RwLock<Option<ListenAddrs>>,
    /// Optional tray handle so reload can refresh token / auth_enabled / listen.
    pub tray: RwLock<Option<bookclerk_tray::SharedTrayConfig>>,
    /// Single-use tray Open Bookclerk ticket (hash + deadline). Replaced on prepare; restart clears it.
    pub tray_handoff: Mutex<Option<TrayHandoffTicket>>,
    /// Guest sessions for deployed instances. One map for the process lifetime.
    pub deployment_runtime: OnceLock<Arc<bookclerk_plugin_host::LiveDeploymentRuntime>>,
    /// Where the node API's TLS listener is bound, once it is. That listener
    /// is bound at startup and outlives config reloads, so a reload that
    /// turns auth off is checked against it as well as the new config.
    pub node_api_tls_bound: OnceLock<std::net::SocketAddr>,
    /// Counts database swaps: bumped after [`Self::library`] is replaced so
    /// the cluster-link supervisor restarts its host clients on the new
    /// store.
    pub library_swaps: tokio::sync::watch::Sender<u64>,
    /// Signs cover URLs into same-origin paths and serves the processed
    /// images (`GET /api/covers/{token}/{size}`).
    pub covers: Arc<crate::covers::CoverService>,
}

/// In-process tray Open Bookclerk ticket: hash of a one-time code plus expiry.
pub struct TrayHandoffTicket {
    /// SHA-256 hex of the plaintext code returned to the tray (never the code itself).
    pub code_hash: String,
    /// Instant after which GET must fail closed.
    pub deadline: Instant,
}

impl AppState {
    /// Cheap clone of the live library handle; drop the read lock before awaiting DB I/O.
    pub async fn library_snapshot(&self) -> LibraryStore {
        self.library.read().await.clone()
    }

    /// Clone the live operator auth `Arc` and drop the lock before further `.await`s.
    pub async fn auth_snapshot(&self) -> Arc<OperatorAuthState> {
        self.auth.read().await.clone()
    }

    /// Reconciler session map for this process. The first call creates it.
    #[must_use]
    pub fn deployment_runtime(&self) -> Arc<bookclerk_plugin_host::LiveDeploymentRuntime> {
        self.deployment_runtime
            .get_or_init(|| {
                Arc::new(bookclerk_plugin_host::LiveDeploymentRuntime::new(
                    Arc::clone(&self.config),
                    Arc::clone(&self.library),
                    Arc::clone(&self.sources),
                    Arc::clone(&self.integrations),
                    Arc::clone(&self.destinations),
                ))
            })
            .clone()
    }
}

#[derive(Clone)]
/// Serves `GET /invite` (validate magic link, then the SPA or a branded 4xx).
struct InvitePageState {
    /// Daemon state for library ticket lookup.
    app: Arc<AppState>,
    /// `ui/dist/index.html` when the GUI is packaged beside the daemon.
    index: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Default, utoipa::ToSchema)]
/// Query string for `GET /invite`.
struct InviteQuery {
    /// Opaque one-time claim ticket from the magic link.
    ticket: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Why `GET /invite` must not render the accept-invite SPA.
enum InviteLinkError {
    /// `?ticket=` missing or blank.
    MissingTicket,
    /// Unknown token (never issued).
    Invalid,
    /// Unredeemed ticket past `expires_at`.
    Expired,
    /// Ticket already consumed.
    AlreadyRedeemed,
}

impl InviteLinkError {
    /// HTTP status for this invite failure.
    fn status(self) -> StatusCode {
        match self {
            Self::MissingTicket | Self::Invalid => StatusCode::BAD_REQUEST,
            Self::Expired | Self::AlreadyRedeemed => StatusCode::GONE,
        }
    }

    /// Operator-facing explanation shown on the branded error page.
    fn message(self) -> &'static str {
        match self {
            Self::MissingTicket => "This invite link is missing a ticket.",
            Self::Invalid => "This invite link is invalid.",
            Self::Expired => "This invite link has expired.",
            Self::AlreadyRedeemed => "This invite link has already been used.",
        }
    }
}

/// Classifies a magic-link ticket for the invite document route.
///
/// # Arguments
///
/// * `ticket` - Raw `?ticket=` value.
/// * `record` - Row loaded by token hash, if any.
/// * `now` - Clock used for expiry.
///
/// # Returns
///
/// `Some` when the accept-invite page must not be served.
fn invite_link_error(
    ticket: Option<&str>,
    record: Option<&ClaimTicketRecord>,
    now: DateTime<Utc>,
) -> Option<InviteLinkError> {
    if ticket.map(str::trim).filter(|s| !s.is_empty()).is_none() {
        return Some(InviteLinkError::MissingTicket);
    }
    match record {
        None => Some(InviteLinkError::Invalid),
        Some(row) if row.redeemed_at.is_some() => Some(InviteLinkError::AlreadyRedeemed),
        Some(row) if row.expires_at <= now => Some(InviteLinkError::Expired),
        Some(_) => None,
    }
}

/// `GET /invite?ticket=` — branded 4xx when the magic link is unusable.
///
/// # Arguments
///
/// * `state` - Library handle plus optional SPA index path.
/// * `q` - Invite query string.
///
/// # Returns
///
/// SPA `index.html` for a live ticket, or branded 4xx HTML.
async fn invite_page(
    State(state): State<InvitePageState>,
    Query(q): Query<InviteQuery>,
) -> Response {
    let ticket = q.ticket.as_deref();
    let record = if let Some(raw) = ticket.map(str::trim).filter(|s| !s.is_empty()) {
        let library = state.app.library_snapshot().await;
        match library.get_claim_ticket_by_hash(&hash_token(raw)).await {
            Ok(row) => row,
            Err(_) => {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    } else {
        None
    };
    if let Some(err) = invite_link_error(ticket, record.as_ref(), Utc::now()) {
        return http_error::document_error(err.status(), err.message());
    }
    let Some(index) = state.index.as_deref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    serve_invite_spa(index).await
}

/// Reads packaged `index.html` for a still-valid invite ticket.
///
/// # Arguments
///
/// * `index` - Absolute path to `ui/dist/index.html`.
///
/// # Returns
///
/// HTML document, or 404 when the file is missing.
async fn serve_invite_spa(index: &Path) -> Response {
    match tokio::fs::read(index).await {
        Ok(bytes) if !bytes.is_empty() => (
            [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/html; charset=utf-8"),
                ),
                (
                    header::CONTENT_SECURITY_POLICY,
                    HeaderValue::from_static(SPA_CSP),
                ),
            ],
            bytes,
        )
            .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
/// Snapshot of one queued or running control-plane job.
pub struct JobInfo {
    /// Opaque job id returned to the client when the job is accepted.
    #[schema(example = "scan-7d0c1e2f-3a4b-4c5d-8e6f-9a0b1c2d3e4f")]
    pub id: String,
    /// What the job does.
    pub kind: bookclerk_library::JobKind,
    /// Where the job is in its lifecycle.
    pub status: bookclerk_library::JobState,
    /// Optional operator-facing progress or error text.
    #[schema(example = "scanning reader@example.com (libro)")]
    pub detail: Option<String>,
    /// Human-readable progress string while the job is running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "12 of 40 titles")]
    pub progress: Option<String>,
    /// Number of times a worker has claimed this row.
    #[serde(default)]
    #[schema(example = 1)]
    pub attempt_count: i64,
    /// Maximum claims before a failure is terminal.
    #[serde(default)]
    #[schema(example = 3)]
    pub max_attempts: i64,
    /// Structured error kind when failed or cancelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "cancelled")]
    pub error_kind: Option<String>,
    /// RFC 3339 timestamp when the row was inserted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "2026-10-29T10:12:00Z")]
    pub created_at: Option<String>,
    /// RFC 3339 timestamp when the row reached a terminal state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "2026-10-29T10:14:22Z")]
    pub finished_at: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// Liveness payload for unauthenticated `GET /health`.
struct HealthResponse {
    /// Always `ok` when the process can serve HTTP.
    #[schema(example = "ok")]
    status: &'static str,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// Library and daemon snapshot for `GET /api/status`.
struct StatusResponse {
    /// Count of configured store accounts.
    #[schema(example = 2)]
    accounts: usize,
    /// Total title rows in the library.
    #[schema(example = 412)]
    books: usize,
    /// Titles whose `acquire_status` is `acquired`.
    #[schema(example = 380)]
    acquired: i64,
    /// Titles still needing acquire (`not_acquired`).
    #[schema(example = 27)]
    pending: i64,
    /// Titles stuck in `error` after a failed acquire.
    #[schema(example = 1)]
    error: i64,
    /// Titles currently `queued` or `downloading`.
    #[schema(example = 4)]
    in_progress: i64,
    /// Comma-joined `daemon.listen` addresses from live config.
    #[schema(example = "127.0.0.1:8787,[::1]:8787")]
    listen: String,
    /// Comma-joined enabled destination plugin ids, or `none`.
    #[schema(example = "local")]
    storage_backend: String,
    /// Whether the live auth middleware requires operator credentials.
    #[schema(example = true)]
    auth_enabled: bool,
    /// Durable domain-event delivery-queue counters.
    events: EventQueueStatus,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// Outbox delivery counts for `GET /api/status`.
struct EventQueueStatus {
    /// Pending deliveries without a suspend checkpoint.
    #[schema(example = 3)]
    pending: i64,
    /// Currently claimed deliveries.
    #[schema(example = 1)]
    running: i64,
    /// Pending deliveries parked with a checkpoint.
    #[schema(example = 0)]
    suspended: i64,
    /// Dead-lettered deliveries.
    #[schema(example = 0)]
    dead_letter: i64,
    /// Successfully acked deliveries.
    #[schema(example = 1284)]
    acked: i64,
    /// Age in seconds of the oldest pending delivery, when any exist.
    #[schema(example = 12)]
    oldest_pending_age_secs: Option<i64>,
    /// Durable retry outcomes (including reclaim-as-retry).
    #[schema(example = 7)]
    retries_total: i64,
    /// Durable suspend outcomes.
    #[schema(example = 2)]
    suspensions_total: i64,
    /// Durable transitions into `dead_letter`.
    #[schema(example = 0)]
    dead_letters_total: i64,
    /// Average first-dispatch latency in milliseconds, when sampled.
    #[schema(example = 140)]
    dispatch_latency_ms_avg: Option<i64>,
    /// Average `onEvent` handler duration in milliseconds, when sampled.
    #[schema(example = 860)]
    handler_latency_ms_avg: Option<i64>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
/// One `config.toml` key/value pair from a settings PATCH.
struct SettingsUpdate {
    /// Dotted config key; rejected unless it passes the settings allowlist.
    #[schema(example = "library.auto_acquire")]
    key: String,
    /// Raw string value, normalized before it is written.
    #[schema(example = "false")]
    value: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
/// Body for `PATCH /api/settings`.
struct PatchSettingsRequest {
    /// Key/value pairs to validate, persist, and reload.
    settings: Vec<SettingsUpdate>,
    /// `core.output` document revision the client read (`output_revision` in
    /// `GET /api/settings`). When set and stale, an `output.*` update is
    /// rejected with 409.
    #[serde(default)]
    #[schema(example = 7)]
    expected_output_revision: Option<i64>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// One selectable value for a Settings dropdown.
struct PluginSettingChoice {
    /// Stored config value; empty string means use the plugin default.
    #[schema(example = "m4b")]
    value: String,
    /// Operator-facing choice caption.
    #[schema(example = "M4B")]
    label: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// One editable setting shown in the Settings UI.
struct PluginSettingOption {
    /// Dotted `config.toml` key for this option.
    #[schema(example = "container")]
    key: String,
    /// Operator-facing field caption.
    #[schema(example = "Container")]
    label: String,
    /// Current string form of the setting.
    #[schema(example = "m4b")]
    value: String,
    /// UI widget hint (`boolean`, `string`, or `number`).
    #[schema(example = "string")]
    value_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional dropdown values; omitted for free-form fields.
    choices: Option<Vec<PluginSettingChoice>>,
    /// True for a `secret` setting: `value` is never the stored value.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    #[schema(example = false)]
    secret: bool,
    /// For a secret setting, whether the instance document names a sealed value.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = true)]
    secret_set: Option<bool>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// Settings UI group for one discovered or loaded plugin.
struct PluginSettingsGroup {
    /// Plugin id (`audible`, `sqlite`, and similar).
    #[schema(example = "libro")]
    id: String,
    /// Handler family wire label (`source`, `integration`, `output`, `database`).
    #[schema(example = "source")]
    family: String,
    /// Provenance-qualified PluginKey when this row came from a discovered install.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "platform:bookclerk/bookclerk-plugin-source-libro#libro")]
    plugin_key: Option<String>,
    /// Host-evaluated provenance (`platform_bundled`, `verified_staged`, `installed`, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "platform_bundled")]
    provenance: Option<String>,
    /// Same-origin logo for Settings list rows: the plugin's embedded logo,
    /// or a 128 px copy of the remote logo its manifest names.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(
        example = "/api/images/aHR0cHM6Ly93d3cuZ29vZ2xlLmNvbS9zMi9mYXZpY29ucz9kb21haW49bGlicm8uZm0mc3o9MTI4.N14rqh66Bok9-eCqJKBNoQ"
    )]
    logo: Option<String>,
    /// Editable options for this plugin, including `.enabled`.
    settings: Vec<PluginSettingOption>,
    /// Plugin instance this group edits; absent for a bootstrap database adapter.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "3f9c2b7e-1d4a-4c8e-9b2f-6a7d8e9f0a1b")]
    plugin_instance_id: Option<String>,
    /// Operator-facing instance name, when the operator set one.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "Libro.fm (household)")]
    display_name: Option<String>,
    /// True when a bare plugin key or alias routes to this instance.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    #[schema(example = true)]
    is_default: bool,
    /// Instance document revision for compare-and-swap writes.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 4)]
    config_revision: Option<i64>,
    /// This node's deployment of the instance, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    deployment: Option<crate::config_authority::PluginDeploymentView>,
    /// Scheduler placement; absent when the instance is hand-managed (the
    /// Enabled switch applies only then).
    #[serde(skip_serializing_if = "Option::is_none")]
    placement: Option<crate::config_authority::PlacementView>,
    /// Nodes whose deployment of the instance is `present`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[schema(example = json!(["6d1f2e3a-4b5c-4d6e-8f7a-9b0c1d2e3f4a"]))]
    present_on: Vec<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// Settings page payload: file values, effective runtime, and plugin groups.
struct SettingsResponse {
    /// Values as written in config.toml (and env overlays), plus `output.*`
    /// from the `core.output` database document.
    #[schema(example = json!({"daemon.auth.enabled": "true", "daemon.listen": "127.0.0.1:8787,[::1]:8787", "library.auto_acquire": "false"}))]
    settings: std::collections::BTreeMap<String, String>,
    /// Runtime-effective values after the last successful reload (auth, plugins).
    #[schema(example = json!({"daemon.auth.enabled": "true", "daemon.listen": "127.0.0.1:8787,[::1]:8787", "library.auto_acquire": "false"}))]
    effective: std::collections::BTreeMap<String, String>,
    /// Per-plugin setting groups rendered by the Settings UI.
    plugins: Vec<PluginSettingsGroup>,
    /// Host max jail CPU in cores (2 d.p.; equals logical CPU count).
    #[schema(example = 8.0)]
    host_cpu_cores_max: f64,
    /// Optional global per-jail CPU ceiling in cores (from `[plugins.jail]`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 2.0)]
    jail_cpu_cores: Option<f64>,
    /// Revision of the `core.output` document the `output.*` values in
    /// `settings` were read from; send it back as `expected_output_revision`.
    /// `effective["output.revision"]` is the revision this host has applied,
    /// which lags the document until the host's next wakeup.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 7)]
    output_revision: Option<i64>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// Optional branding shown on the plugin consent dialog.
struct PluginConsentBrand {
    /// Display name from the portal brand or plugin manifest.
    #[schema(example = "Libro.fm")]
    name: String,
    /// Background CSS color from the portal brand, when known.
    #[schema(example = "#0f2a3f")]
    bg: Option<String>,
    /// Foreground CSS color from the portal brand, when known.
    #[schema(example = "#ffffff")]
    fg: Option<String>,
    /// Accent CSS color from the portal brand, when known.
    #[schema(example = "#f4b400")]
    accent: Option<String>,
    /// Same-origin logo: the host-served embed path, or a 128 px copy of
    /// the remote logo the plugin names.
    #[schema(example = "/api/plugins/source/libro/logo")]
    logo: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// Host-capped resource defaults shown on the consent dialog.
struct PluginConsentLimits {
    /// Requested workerd CPU budget in milliseconds.
    #[schema(example = 30000)]
    cpu_ms: u32,
    /// Requested workerd subrequest budget.
    #[schema(example = 50)]
    subrequests: u32,
    /// Host hard cap for workerd CPU milliseconds.
    #[schema(example = 120000)]
    max_cpu_ms: u32,
    /// Host hard cap for workerd subrequests.
    #[schema(example = 1000)]
    max_subrequests: u32,
    /// Default plugin state disk budget in mebibytes.
    #[schema(example = 512)]
    disk_mib: u32,
    /// Host hard cap for plugin state disk in mebibytes.
    #[schema(example = 4096)]
    max_disk_mib: u32,
    /// Default jail memory in mebibytes.
    #[schema(example = 512)]
    memory_mib: u32,
    /// Host hard cap for jail memory in mebibytes.
    #[schema(example = 4096)]
    max_memory_mib: u32,
    /// Manifest / default jail CPU in cores (2 d.p.).
    #[schema(example = 0.8)]
    cpu_cores: f64,
    /// Host max jail CPU in cores (2 d.p.).
    #[schema(example = 8.0)]
    max_cpu_cores: f64,
    /// Optional `[plugins.jail]` ceiling in cores.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 2.0)]
    jail_cpu_cores: Option<f64>,
    /// Default extra process/thread budget (native consent).
    #[schema(example = 2)]
    extra_processes: u32,
    /// Host hard cap for extra process budget.
    #[schema(example = 62)]
    max_extra_processes: u32,
    /// Suggested / default host bindings operators may grant.
    #[schema(example = json!(["config", "secrets", "plugin_kv", "work_fs", "oauth"]))]
    known_bindings: Vec<&'static str>,
}

/// Operator-facing grant JSON: CPU as cores (not Spec percent).
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct PluginGrantView {
    /// Canonical provenance-qualified PluginKey.
    #[schema(example = "platform:bookclerk/bookclerk-plugin-source-libro#libro")]
    plugin_key: String,
    /// Plugin id this grant applies to.
    #[schema(example = "libro")]
    plugin_id: String,
    /// Exported entrypoints the operator approved (`storefront`, `cli`, …).
    #[schema(example = json!(["storefront"]))]
    entrypoints: Vec<String>,
    /// Event types the operator approved the plugin to publish.
    #[schema(example = json!(["book_acquired"]))]
    producers: Vec<String>,
    /// Approved event-consumer subscriptions.
    #[schema(value_type = Vec<Object>, example = json!([{"eventType": "book_acquired", "schemaVersions": [1], "supportsSuspend": false}]))]
    consumers: Vec<GrantedEventConsumer>,
    /// Approved job trigger types.
    #[schema(example = json!(["stream_copy"]))]
    jobs: Vec<String>,
    /// Approved egress mode (`deny` or `outbound`).
    #[schema(example = "outbound")]
    network_mode: String,
    /// Hostnames the operator approved for egress.
    #[schema(example = json!(["*.libro.fm", "api.libro.fm"]))]
    domains: Vec<String>,
    /// Destinations requested by the current manifest.
    #[schema(example = json!(["*.libro.fm"]))]
    manifest_domains: Vec<String>,
    /// Operator-added destinations beyond the manifest.
    #[schema(example = json!(["api.libro.fm"]))]
    operator_added_domains: Vec<String>,
    /// Operator-denied destinations still listed by the manifest.
    #[schema(example = json!([]))]
    operator_denied_domains: Vec<String>,
    /// Effective raw TCP grants.
    tcp: Vec<TcpGrantView>,
    /// Operator-added TCP grants.
    operator_added_tcp: Vec<TcpGrantView>,
    /// Operator-denied TCP grants still listed by the manifest.
    operator_denied_tcp: Vec<TcpGrantView>,
    /// Effective address-space CIDRs.
    #[schema(example = json!(["203.0.113.0/24"]))]
    address_cidrs: Vec<String>,
    /// Operator-added CIDRs.
    #[schema(example = json!(["203.0.113.0/24"]))]
    operator_added_cidrs: Vec<String>,
    /// Operator-denied CIDRs still listed by the manifest.
    #[schema(example = json!([]))]
    operator_denied_cidrs: Vec<String>,
    /// Undeclared public redirect permission.
    #[schema(example = false)]
    allow_undeclared_public_redirects: bool,
    /// Hosts (exact or `*.` wildcard) that receive the plugin's own
    /// `User-Agent`; every other request carries the host's agent. Absent on
    /// a grant approved before this permission existed, which keeps the hosts
    /// the plugin's manifest declares.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(["*.libro.fm"]))]
    own_user_agent_hosts: Option<Vec<String>>,
    /// The plugin's reason for [`Self::own_user_agent_hosts`], from
    /// `plugin.toml`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(
        example = "Libro.fm's sign-in, library and download API answers the HTTP client of the Libro.fm Android app."
    )]
    own_user_agent_reason: Option<String>,
    /// SHA-256 of persisted operator consent (no host overlays).
    #[schema(example = "9b74c9897bac770ffc029102a200c5de3a9c4a1d6c2f1e0b5a7d8c9e0f1a2b3c")]
    grant_revision: String,
    /// SHA-256 of the canonical effective authority.
    #[schema(example = "1f3870be274f6c49b3e31a0c6728957f4e1d5b2c7a8d9e0f1a2b3c4d5e6f7a8b")]
    authority_revision: String,
    /// Host bindings the operator approved.
    #[schema(example = json!(["config", "secrets"]))]
    bindings: Vec<String>,
    /// Extra compatibility flags stored on the grant.
    #[schema(example = json!(["nodejs_compat"]))]
    compatibility_flags: Vec<String>,
    /// RFC 3339 timestamp when the grant was last approved.
    #[schema(example = "2026-10-29T10:12:00Z")]
    approved_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional workerd CPU budget in milliseconds.
    #[schema(example = 50)]
    cpu_ms: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional workerd subrequest budget.
    #[schema(example = 50)]
    subrequests: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional plugin state disk budget in mebibytes.
    #[schema(example = 256)]
    disk_mib: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional jail memory in mebibytes.
    #[schema(example = 512)]
    memory_mib: Option<u32>,
    /// Jail CPU in cores (2 d.p.); absent for workerd.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 1.0)]
    cpu_cores: Option<f64>,
    /// Extra process/thread budget beyond launcher overhead; native only.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 4)]
    extra_processes: Option<u32>,
}

/// Operator-facing TCP grant (`host` + `ports`).
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct TcpGrantView {
    /// Hostname or `*.` wildcard.
    #[schema(example = "*.libro.fm")]
    host: String,
    /// Destination ports.
    #[schema(example = json!([443]))]
    ports: Vec<u16>,
}

impl From<&bookclerk_plugin_host::TcpGrant> for TcpGrantView {
    fn from(grant: &bookclerk_plugin_host::TcpGrant) -> Self {
        Self {
            host: grant.host.clone(),
            ports: grant.ports.clone(),
        }
    }
}

impl PluginGrantView {
    /// Projects a stored grant into operator-facing camelCase JSON, converting CPU percent to cores.
    fn from_grant(grant: &PluginGrant) -> Self {
        Self {
            plugin_key: grant.plugin_key.clone(),
            plugin_id: grant.plugin_id.clone(),
            entrypoints: grant.entrypoints.iter().cloned().collect(),
            producers: grant.producers.iter().cloned().collect(),
            consumers: grant.consumers.iter().cloned().collect(),
            jobs: grant.jobs.iter().cloned().collect(),
            network_mode: grant.network_mode.clone(),
            domains: grant.domains.iter().cloned().collect(),
            manifest_domains: grant.manifest_domains.iter().cloned().collect(),
            operator_added_domains: grant.operator_added_domains.iter().cloned().collect(),
            operator_denied_domains: grant.operator_denied_domains.iter().cloned().collect(),
            tcp: grant.tcp.iter().map(TcpGrantView::from).collect(),
            operator_added_tcp: grant
                .operator_added_tcp
                .iter()
                .map(TcpGrantView::from)
                .collect(),
            operator_denied_tcp: grant
                .operator_denied_tcp
                .iter()
                .map(TcpGrantView::from)
                .collect(),
            address_cidrs: grant.address_cidrs.iter().cloned().collect(),
            operator_added_cidrs: grant.operator_added_cidrs.iter().cloned().collect(),
            operator_denied_cidrs: grant.operator_denied_cidrs.iter().cloned().collect(),
            allow_undeclared_public_redirects: grant.allow_undeclared_public_redirects,
            own_user_agent_hosts: grant
                .own_user_agent_hosts
                .as_ref()
                .map(|hosts| hosts.iter().cloned().collect()),
            own_user_agent_reason: grant.own_user_agent_reason.clone(),
            grant_revision: bookclerk_plugin_host::grant_revision(grant),
            authority_revision: bookclerk_plugin_host::authority_revision(grant),
            bindings: grant.bindings.iter().cloned().collect(),
            compatibility_flags: grant.compatibility_flags.iter().cloned().collect(),
            approved_at: grant.approved_at.clone(),
            cpu_ms: grant.cpu_ms,
            subrequests: grant.subrequests,
            disk_mib: grant.disk_mib,
            memory_mib: grant.memory_mib,
            cpu_cores: grant.cpu_rate_percent.map(percent_to_cores),
            extra_processes: grant.extra_processes,
        }
    }
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// Consent dialog payload for one plugin.
struct PluginConsentResponse {
    /// Plugin id whose grant is being reviewed.
    #[schema(example = "libro")]
    plugin_id: String,
    /// Canonical PluginKey for this install (empty when unknown).
    #[schema(example = "platform:bookclerk/bookclerk-plugin-source-libro#libro")]
    plugin_key: String,
    /// Host-evaluated provenance label.
    #[schema(example = "platform_bundled")]
    provenance: String,
    /// Guest runtime (`native` or `workerd`) — drives which controls are enforceable.
    #[schema(example = "native")]
    runtime: String,
    /// Manifest-derived grant the plugin is asking for.
    request: PluginGrantView,
    /// True when an existing on-disk grant already satisfies the request.
    #[schema(example = false)]
    covered: bool,
    /// Operator-facing bullet list of requested capabilities.
    #[schema(example = json!(["Plugin: libro (entrypoints: storefront)", "Network: outbound", "Raw TCP connect: *.libro.fm:[443]", "Host bindings: config, secrets"]))]
    summary: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Previously approved grant, when one is stored.
    existing: Option<PluginGrantView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// What the request asks for beyond the stored grant (a plugin upgrade
    /// that widened its capabilities); absent when no grant is stored. The
    /// plugin keeps running under the stored grant until these are approved.
    pending: Option<PluginConsentPending>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional portal or manifest branding for the dialog.
    brand: Option<PluginConsentBrand>,
    /// Host-capped resource defaults (workerd budgets + shared disk). Always set.
    limits: PluginConsentLimits,
    /// The `User-Agent` this host sends for the plugin's requests, except to
    /// the request's `ownUserAgentHosts`.
    #[schema(example = "bookclerk/0.1.0 (+https://bookclerk.app)")]
    host_user_agent: String,
}

/// The difference between a plugin's request and its stored grant.
#[derive(Debug, Clone, Default, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct PluginConsentPending {
    /// Operator-facing lines naming every capability awaiting consent, the
    /// same lines `bookclerk plugins info` and `bookclerk plugins approve`
    /// print; empty when the stored grant covers the request.
    #[schema(example = json!(["Awaiting consent (not automatically approved):", "  fetch hosts: any public website (*)"]))]
    lines: Vec<String>,
    /// Fetch host patterns the request adds (`*` is any public host).
    #[schema(example = json!(["*"]))]
    domains: Vec<String>,
    /// TCP `host:ports` the request adds, narrowed to the new ports.
    tcp: Vec<TcpGrantView>,
    /// CIDRs the request adds.
    #[schema(example = json!(["192.168.1.0/24"]))]
    address_cidrs: Vec<String>,
    /// Whether the request newly asks to follow undeclared public redirects.
    #[schema(example = false)]
    allow_undeclared_public_redirects: bool,
}

impl From<&bookclerk_plugin_host::PendingStructural> for PluginConsentPending {
    fn from(pending: &bookclerk_plugin_host::PendingStructural) -> Self {
        Self {
            lines: pending.summary_lines(),
            domains: pending.domains.iter().cloned().collect(),
            tcp: pending.tcp.iter().map(TcpGrantView::from).collect(),
            address_cidrs: pending.address_cidrs.iter().cloned().collect(),
            allow_undeclared_public_redirects: pending.undeclared_public_redirects,
        }
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
/// Body for `POST /api/plugins/{id}/consent`.
struct PluginConsentApproveRequest {
    /// Must be true; a false value is rejected with 400.
    #[schema(example = true)]
    approve: bool,
    #[serde(default)]
    /// Optional operator overrides applied on top of the manifest request.
    grant: Option<PluginGrantOverride>,
}

#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
/// Operator-supplied grant fields that replace matching request values.
struct PluginGrantOverride {
    /// Override egress mode (`deny` or `outbound`) when set.
    #[schema(example = "outbound")]
    network_mode: Option<String>,
    /// Override approved hostnames when set.
    #[schema(example = json!(["*.libro.fm", "api.libro.fm"]))]
    domains: Option<Vec<String>>,
    /// Override raw TCP grants when set.
    tcp: Option<Vec<TcpGrantView>>,
    /// Override address-space CIDRs when set.
    #[schema(example = json!(["203.0.113.0/24"]))]
    address_cidrs: Option<Vec<String>>,
    /// Override undeclared-public-redirect permission when set.
    #[schema(example = false)]
    allow_undeclared_public_redirects: Option<bool>,
    /// Override the hosts that keep the plugin's own `User-Agent` when set.
    ///
    /// Each entry must be one the request lists; leaving one out sends the
    /// host's agent there instead.
    #[schema(example = json!(["*.libro.fm"]))]
    own_user_agent_hosts: Option<Vec<String>>,
    /// Override approved bindings when set.
    ///
    /// Entries are host binding names (`config`, `secrets`, `plugin_kv`,
    /// `work_fs`, `oauth`) and the manifest tokens the consent request lists
    /// (`database:<NAME>`, `queue:<BINDING>`, `queue_consumer:<queue>`,
    /// `cron:<schedule>`). A manifest token must match one in the request
    /// exactly; leaving one out narrows the grant.
    #[schema(example = json!(["config", "secrets", "queue:NOTIFY", "queue_consumer:jobs"]))]
    bindings: Option<Vec<String>>,
    /// Override compatibility flags when set.
    #[schema(example = json!(["nodejs_compat"]))]
    compatibility_flags: Option<Vec<String>>,
    /// Override workerd CPU milliseconds when set.
    #[schema(example = 100)]
    cpu_ms: Option<u32>,
    /// Override workerd subrequest budget when set.
    #[schema(example = 100)]
    subrequests: Option<u32>,
    /// Override disk budget in mebibytes when set.
    #[schema(example = 512)]
    disk_mib: Option<u32>,
    /// Override jail memory in mebibytes when set.
    #[schema(example = 1024)]
    memory_mib: Option<u32>,
    /// Jail CPU in cores (2 d.p.); converted to Spec percent server-side.
    #[schema(example = 1.5)]
    cpu_cores: Option<f64>,
    /// Extra process/thread budget (native); ignored for workerd.
    #[schema(example = 8)]
    extra_processes: Option<u32>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// Generic mutation result with success flag, message, and optional job id.
struct ActionResponse {
    /// True when the action was accepted or completed.
    #[schema(example = true)]
    ok: bool,
    /// Operator-facing status text.
    #[schema(example = "scan queued")]
    message: String,
    /// Queued job id, or empty when the action is synchronous.
    #[schema(example = "scan-7d0c1e2f-3a4b-4c5d-8e6f-9a0b1c2d3e4f")]
    job_id: String,
    /// Machine slug when admission failed (`conflict`, `queue_full`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "queue_full")]
    error: Option<String>,
}

#[derive(Debug, Deserialize, Default, utoipa::ToSchema)]
/// Optional body for `POST /api/library/scan`.
pub struct ScanRequest {
    /// When set, scan this account even if `scan_enabled` is false.
    #[schema(example = "reader@example.com")]
    pub account: Option<String>,
    /// When set, scan only this deployed plugin instance.
    #[schema(example = "3f9c2b7e-1d4a-4c8e-9b2f-6a7d8e9f0a1b")]
    pub plugin_instance_id: Option<String>,
}

#[derive(Debug, Deserialize, Default, utoipa::ToSchema)]
/// Optional body for `POST /api/library/acquire`.
pub struct AcquireRequestBody {
    /// Audible ASIN filter; first of uuid/asin/isbn/product_id wins.
    #[schema(example = "B0C8HVKW7X")]
    pub asin: Option<String>,
    /// Library title UUID filter.
    #[schema(example = "3f2c0d7e-9d4b-4a62-8b1e-6d0f7c2a9b31")]
    pub uuid: Option<String>,
    /// ISBN filter for non-Audible stores.
    #[schema(example = "9781797176802")]
    pub isbn: Option<String>,
    /// Store product id filter.
    #[schema(example = "1797176803")]
    pub product_id: Option<String>,
    /// Restrict acquire to this store account.
    #[schema(example = "reader@example.com")]
    pub account: Option<String>,
    /// When set, fetch through this deployed plugin instance.
    #[schema(example = "3f9c2b7e-1d4a-4c8e-9b2f-6a7d8e9f0a1b")]
    pub plugin_instance_id: Option<String>,
    /// More title ids; each (with the single filter above) becomes its own
    /// job.
    #[serde(default)]
    #[schema(example = json!(["8c1d2e3f-4a5b-4c6d-9e7f-0a1b2c3d4e5f"]))]
    pub titles: Vec<String>,
    /// Re-download titles already acquired.
    #[serde(default)]
    #[schema(example = false)]
    pub force: bool,
    /// Fetch only each title's companion PDF.
    #[serde(default)]
    #[schema(example = false)]
    pub pdf_only: bool,
}

#[derive(Debug, Deserialize, Default, utoipa::ToSchema)]
/// Optional body for `POST /integrations/{id}/scan`.
pub struct IntegrationScanRequest {
    /// When true, the integration rescans even if it considers itself current.
    #[schema(example = true)]
    pub force: Option<bool>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
/// Body for copying library rows between database plugins.
struct DatabaseMigrateRequest {
    /// Source plugin id. Defaults to the active `[database].plugin` before reload.
    #[schema(example = "sqlite")]
    from: Option<String>,
    /// Destination database plugin id to copy into.
    #[schema(example = "postgres")]
    to: String,
    #[serde(default)]
    /// When true, count rows without writing.
    #[schema(example = true)]
    dry_run: bool,
    #[serde(default)]
    /// When true, overwrite a destination that is not empty.
    #[schema(example = false)]
    force: bool,
    /// Update `[database].plugin` in config.toml after a successful copy.
    #[serde(default)]
    #[schema(example = false)]
    apply: bool,
}

#[derive(Debug, Deserialize, Default, utoipa::ToSchema, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
/// Query string for `GET /api/library/books`.
struct BooksQuery {
    /// Restrict results to this store account id.
    account: Option<String>,
    /// Optional `AcquireStatus` wire-string filter.
    status: Option<String>,
    /// Full-text search over this node's search index. Hits are sorted by
    /// title and every hit counts toward `total` (there is no hit cap); an
    /// empty value skips the index.
    q: Option<String>,
    /// Page size, clamped to 1–500 (default 40).
    limit: Option<usize>,
    /// Number of matching titles to skip.
    offset: Option<usize>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// Paginated library listing.
struct BooksResponse {
    /// Current page of title records.
    books: Vec<BookRecord>,
    /// Match count before pagination.
    #[schema(example = 412)]
    total: usize,
    /// Applied page size.
    #[schema(example = 50)]
    limit: usize,
    /// Applied skip count.
    #[schema(example = 0)]
    offset: usize,
}

/// Every documented route group served under the root, with who may call it.
///
/// The groups carry no state or auth layers: [`router`] wraps each in the
/// middleware its [`Access`] names ([`guarded`]), and the OpenAPI document
/// ([`documented_groups`]) reads the same groups, so a served route always
/// has its spec entry and states the access that is enforced.
///
/// # Returns
///
/// The groups from this module and the auth, OIDC, passkey and TOTP modules.
// The legacy aliases are deprecated in the document and still served.
#[allow(deprecated)]
pub(crate) fn route_groups() -> Vec<RouteGroup> {
    // Use `route_layer` (not `layer`) for auth, so auth never wraps the
    // router's fallback: `.layer(auth)` made unmatched paths, including `/`
    // when the GUI dist was missing, return a bare 401 instead of serving the
    // SPA or the branded 404.
    let operator_or_admin = OpenApiRouter::new()
        .routes(routes!(legacy_status))
        .routes(routes!(legacy_scan))
        .routes(routes!(legacy_acquire))
        .routes(routes!(legacy_list_jobs))
        .routes(routes!(legacy_cancel_job))
        .routes(routes!(trigger_integration_scan))
        .routes(routes!(status))
        .routes(routes!(list_jobs))
        .routes(routes!(cancel_job))
        .routes(routes!(list_events))
        .routes(routes!(list_event_deliveries))
        .routes(routes!(list_queues))
        .routes(routes!(inspect_queue))
        .routes(routes!(retry_event_delivery))
        .routes(routes!(ack_event_delivery))
        .routes(routes!(cancel_event_delivery))
        .routes(routes!(resume_event_delivery))
        .routes(routes!(sync_listening));

    let operator_only = OpenApiRouter::new()
        .routes(routes!(reload_config))
        .routes(routes!(
            crate::config_authority::get_events_domain,
            crate::config_authority::put_events_domain
        ))
        .routes(routes!(
            crate::config_authority::get_output_domain,
            crate::config_authority::put_output_domain
        ))
        .routes(routes!(
            crate::config_authority::get_plugin_instances,
            crate::config_authority::post_plugin_instance
        ))
        .routes(routes!(
            crate::config_authority::delete_plugin_instance_route
        ))
        .routes(routes!(
            crate::config_authority::get_plugin_instance_config,
            crate::config_authority::put_plugin_instance_config
        ))
        .routes(routes!(crate::config_authority::put_plugin_deployment))
        .routes(routes!(
            crate::config_authority::put_plugin_placement_route,
            crate::config_authority::delete_plugin_placement_route
        ))
        .routes(routes!(crate::config_authority::put_node_labels))
        .routes(routes!(crate::config_authority::put_node_owner))
        .routes(routes!(
            crate::plugin_installs::get_plugins,
            crate::plugin_installs::post_plugin_install
        ))
        .routes(routes!(
            crate::plugin_installs::get_plugin,
            crate::plugin_installs::delete_plugin_install
        ))
        .routes(routes!(crate::plugin_installs::put_active_version))
        .routes(routes!(crate::plugin_installs::approve_plugin_version))
        .routes(routes!(crate::cluster_link::api::get_cluster_consents))
        .routes(routes!(crate::cluster_link::api::post_cluster_scan))
        .routes(routes!(crate::cluster_link::api::post_cluster_jobs))
        .routes(routes!(
            crate::cluster_link::api::post_cluster_consent_approve
        ))
        .routes(routes!(
            crate::cluster_link::api::post_cluster_consent_revoke
        ))
        // Join a library / Leave library (the welcome screen and wizard).
        .routes(routes!(crate::cluster_link::join::get_join_status))
        .routes(routes!(crate::cluster_link::join::post_join_inspect))
        .routes(routes!(crate::cluster_link::join::post_join_start))
        .routes(routes!(crate::cluster_link::join::post_join_callback))
        .routes(routes!(crate::cluster_link::join::post_join_invite))
        .routes(routes!(crate::cluster_link::join::post_join_leave))
        .routes(routes!(get_settings, patch_settings))
        .routes(routes!(get_plugin_consent, post_plugin_consent))
        .routes(routes!(get_plugin_logo))
        .routes(routes!(migrate_database))
        .routes(routes!(auth::impersonate, auth::impersonate_end))
        .routes(routes!(auth::elevate_end));

    // Node administration: operators see and edit every node; a user who
    // owns nodes (Host, Administrator, Owner) sees and edits only those. An
    // Owner also sees and removes every owner node (the clerks).
    let node_admin = OpenApiRouter::new()
        .routes(routes!(crate::config_authority::get_nodes))
        .routes(routes!(crate::config_authority::delete_owner_node))
        // Settings → Clerks: every owner node for the operator or an Owner.
        .routes(routes!(crate::config_authority::get_clerks))
        .routes(routes!(crate::config_authority::put_node_runtime))
        .routes(routes!(crate::config_authority::get_cluster_status));

    // Host enrollment links: any signed-in user acts for themselves; the
    // library lets the operator or an Owner act for others.
    let user_scoped = OpenApiRouter::new()
        // Any active user queues scans and acquires; the planner limits a
        // Member or Host to the store accounts linked to them.
        .routes(routes!(trigger_scan))
        .routes(routes!(trigger_acquire))
        .routes(routes!(
            crate::node_api::get_node_invites,
            crate::node_api::post_node_invite
        ))
        .routes(routes!(crate::node_api::delete_node_invite))
        // A branch's sign-in consent ("Let <branch> join <library> as
        // yours?"): a signed-in user, never the operator alone.
        .routes(routes!(
            crate::node_api::branch::get_branch_consent,
            crate::node_api::branch::post_branch_consent
        ))
        // A clerk consent's fresh passkey step (an Owner with a passkey).
        .routes(routes!(
            crate::node_api::branch::post_branch_consent_step_up
        ))
        // Settings → Branches: the library's join settings (operator or
        // Owner; the handler checks).
        .routes(routes!(
            crate::node_api::branch::get_branches,
            crate::node_api::branch::put_branches
        ))
        .routes(routes!(crate::node_api::branch::post_remove_branch_titles))
        // Settings → Library's two sharing settings on a branch (operator
        // or Owner; the handler checks).
        .routes(routes!(
            crate::cluster_link::join::get_join_sharing,
            crate::cluster_link::join::post_join_sharing
        ))
        // Store connections across linked libraries: the branch's person
        // only (the handler checks).
        .routes(routes!(
            crate::cluster_link::connections::post_join_connections
        ))
        .routes(routes!(
            crate::cluster_link::connections::post_join_connections_disconnect
        ))
        // The library lets the operator or an Owner revoke any host and
        // anyone else only the hosts they own.
        .routes(routes!(crate::node_api::delete_node_credential))
        // Any user reads the switch; the handler lets only the operator or
        // an active Owner change it.
        .routes(routes!(
            crate::node_api::get_host_enrollment,
            crate::node_api::put_host_enrollment
        ));

    let shared = OpenApiRouter::new()
        .routes(routes!(list_books))
        .routes(routes!(get_book))
        .routes(routes!(get_book_cover))
        .routes(routes!(crate::covers::get_cover))
        .routes(routes!(crate::covers::get_image))
        .routes(routes!(crate::covers::get_gravatar))
        .routes(routes!(crate::covers::get_store_icon))
        .routes(routes!(discover_status))
        .routes(routes!(discover_refresh))
        .routes(routes!(discover_recommendations))
        .routes(routes!(discover_purchase_hints))
        .routes(routes!(discover_purchase_hints_batch))
        .routes(routes!(discover_catalog_search))
        .routes(routes!(discover_title_meta))
        .routes(routes!(discover_title_meta_batch))
        .routes(routes!(discover_title_reviews))
        .routes(routes!(list_wishlist, create_wishlist))
        .routes(routes!(delete_wishlist))
        .routes(routes!(list_request_queue))
        .routes(routes!(auth::logout))
        .routes(routes!(auth::list_sessions))
        .routes(routes!(auth::revoke_session))
        .routes(routes!(crate::profile::patch_profile))
        .routes(routes!(
            crate::profile::get_catalog_listing,
            crate::profile::put_catalog_listing
        ))
        .routes(routes!(
            crate::profile::put_avatar,
            crate::profile::delete_avatar
        ))
        .routes(routes!(crate::profile::get_avatar))
        .routes(routes!(get_preferences, patch_preferences));

    // `/api/auth/me` stays public so the SPA bootstrap can probe session state
    // without a bare middleware 401 (which some browsers render as an error
    // document). The handler itself reports operator / portal / anonymous.
    let public = OpenApiRouter::new()
        .routes(routes!(health))
        .routes(routes!(auth::login))
        .routes(routes!(auth::signin_methods))
        .routes(routes!(auth::tray_handoff_prepare))
        .routes(routes!(auth::tray_handoff))
        .routes(routes!(auth::me))
        // The SPA's live socket checks Origin and the caller itself.
        .routes(routes!(crate::live::get_live))
        .routes(routes!(auth::elevate))
        .routes(routes!(auth::bootstrap))
        .routes(routes!(auth::password_login, auth::set_password))
        .routes(routes!(auth::list_users, auth::create_user))
        .routes(routes!(auth::patch_user, auth::delete_user))
        .routes(routes!(auth::create_user_claim_ticket))
        .routes(routes!(auth::reset_user_password));

    let mut groups = vec![
        RouteGroup::new(public, Access::Public),
        RouteGroup::new(operator_or_admin, Access::OperatorOrAdministrator),
        RouteGroup::new(operator_only, Access::Operator),
        RouteGroup::new(node_admin, Access::OperatorOrNodeOwner),
        RouteGroup::new(user_scoped, Access::OperatorOrUser),
        RouteGroup::new(shared, Access::OperatorOrPortal),
    ];
    // OIDC authorization server (ABS / third-party user tokens).
    groups.extend(crate::oidc::route_groups());
    // Optional upstream OIDC/OAuth relying party (identity broker).
    groups.extend(crate::oidc_rp::route_groups());
    // WebAuthn passkeys (login + Owner elevate).
    groups.extend(crate::passkeys::route_groups());
    // TOTP MFA + host second-factor policy.
    groups.extend(crate::totp::route_groups());
    groups
}

/// Wraps a route group in the auth middleware its access names.
///
/// # Arguments
///
/// * `routes` - The group.
/// * `access` - Who may call it.
/// * `state` - Daemon state the middleware reads.
///
/// # Returns
///
/// The group with its auth layer on its routes only (`route_layer`).
fn guarded(
    routes: OpenApiRouter<Arc<AppState>>,
    access: Access,
    state: &Arc<AppState>,
) -> OpenApiRouter<Arc<AppState>> {
    let state = state.clone();
    match access {
        Access::Public => routes,
        Access::Operator => routes.route_layer(middleware::from_fn_with_state(
            state,
            auth::require_operator_auth,
        )),
        Access::OperatorOrAdministrator => routes.route_layer(middleware::from_fn_with_state(
            state,
            auth::require_operator_or_administrator_auth,
        )),
        Access::OperatorOrOwner => routes.route_layer(middleware::from_fn_with_state(
            state,
            auth::require_operator_or_owner_auth,
        )),
        Access::OperatorOrNodeOwner => routes.route_layer(middleware::from_fn_with_state(
            state,
            auth::require_operator_or_node_owner_auth,
        )),
        Access::OperatorOrUser => routes.route_layer(middleware::from_fn_with_state(
            state,
            auth::require_operator_or_user_auth,
        )),
        Access::OperatorOrPortal => routes.route_layer(middleware::from_fn_with_state(
            state,
            auth::require_operator_or_portal_auth,
        )),
    }
}

/// The route groups the OpenAPI document covers, in document order.
///
/// # Returns
///
/// [`route_groups`], the node API, this document's own route and the
/// Accounts (portal) routes nested at `/api/portal`.
pub(crate) fn documented_groups() -> Vec<DocumentedGroup> {
    let mut out: Vec<DocumentedGroup> = route_groups()
        .into_iter()
        .map(|g| DocumentedGroup::new(g.routes, g.access))
        .collect();
    out.push(DocumentedGroup::new(
        crate::node_api::node_routes(),
        Access::Public,
    ));
    out.push(DocumentedGroup::new(
        OpenApiRouter::<Arc<utoipa::openapi::OpenApi>>::new()
            .routes(routes!(crate::openapi::get_openapi)),
        Access::Public,
    ));
    out.push(DocumentedGroup::new(
        OpenApiRouter::<crate::openapi::V30Document>::new()
            .routes(routes!(crate::openapi::get_openapi_v30)),
        Access::Public,
    ));
    out.push(DocumentedGroup::new(portal_spa_routes(), Access::Public).nested("/api/portal"));
    out
}

/// Build the HTTP router (API + optional static UI + SPA portal APIs).
pub fn router(state: Arc<AppState>, ui_dist: Option<PathBuf>) -> Router {
    let portal_state = PortalState {
        config: state.config.clone(),
        library: state.library.clone(),
        integrations: state.integrations.clone(),
        sources: state.sources.clone(),
        image_src: {
            let covers = Arc::clone(&state.covers);
            Arc::new(move |url: &str| covers.thumb_src(url))
        },
    };

    // The document is built from the same route groups the router serves.
    let openapi_doc = crate::openapi::shared_document();
    let mut app: Router = Router::new();
    for group in route_groups() {
        let (routes, _) = guarded(group.routes, group.access, &state)
            .with_state(state.clone())
            .split_for_parts();
        app = app.merge(routes);
    }
    // Host-node enrollment and socket: never behind the cookie/auth
    // layers; the transport policy runs first.
    app = app.merge(crate::node_api::node_router(state.clone()));
    app = app.merge(
        OpenApiRouter::new()
            .routes(routes!(crate::openapi::get_openapi))
            .with_state(openapi_doc)
            .split_for_parts()
            .0,
    );
    app = app.merge(
        OpenApiRouter::new()
            .routes(routes!(crate::openapi::get_openapi_v30))
            .with_state(crate::openapi::shared_v30_document())
            .split_for_parts()
            .0,
    );

    // SPA Accounts / claim APIs (Path=/ portal session cookie).
    app = app.nest(
        "/api/portal",
        bookclerk_integrations::portal_spa_router(portal_state),
    );

    let invite_index = ui_dist.as_ref().map(|d| d.join("index.html"));
    app = app.merge(
        Router::new()
            .route("/invite", get(invite_page))
            .with_state(InvitePageState {
                app: state.clone(),
                index: invite_index,
            }),
    );

    // CSRF for cookie-authenticated mutating /api/* (after routes registered).
    app = app.layer(middleware::from_fn_with_state(
        state,
        crate::csrf::require_csrf_for_cookie_api,
    ));

    if let Some(dist) = ui_dist {
        if dist.is_dir() {
            let index = dist.join("index.html");
            // SPA document routes (History API). Real files under dist (assets,
            // favicons, …) are served by ServeDir; unknown paths stay a 404 so
            // the brand-error middleware can render HTML/JSON — do not map every
            // path onto index.html.
            tracing::info!(path = %dist.display(), "serving GUI static assets");
            const SPA_DOC_PATHS: &[&str] = &[
                "/",
                "/discover",
                "/library",
                "/wishlist",
                "/accounts",
                // The Status page. `/status` stays the legacy JSON route.
                "/cluster",
                "/settings",
                // Join a library: the owner's consent page and the
                // joining install's callback.
                "/join/approve",
                "/join/callback",
            ];
            let spa_index = axum::routing::get_service(ServeFile::new(index.clone()))
                .layer(middleware::from_fn(spa_document_csp));
            for path in SPA_DOC_PATHS {
                app = app.route_service(path, spa_index.clone());
            }
            let spa_files = axum::routing::get_service(ServeDir::new(dist))
                .layer(middleware::from_fn(spa_document_csp))
                .layer(middleware::map_response(crate::cache_policy::mark_static));
            app = app.fallback_service(spa_files);
        } else {
            tracing::warn!(
                path = %dist.display(),
                "GUI dist path missing; static UI not served (build ui/ or set BOOKCLERK_UI_DIST)"
            );
        }
    }

    // Timeout wraps API handlers. Brand empty 4xx/5xx bodies (auth, missing
    // routes, handler StatusCode). Cache-Control by response class, outside
    // the branding so rewritten error pages get one too. Normalize trailing
    // slashes before matching. Every response, branded errors included, is
    // closed to other origins (`cross_origin`). Trace outermost.
    let app = app
        .layer(middleware::from_fn(api_timeout_middleware))
        .layer(middleware::from_fn(http_error::brand_error_responses))
        .layer(middleware::from_fn(crate::cache_policy::apply))
        .layer(middleware::from_fn(crate::cross_origin::apply));
    // Test builds compare every response with the OpenAPI document, after
    // the branding so the body compared is the one a client receives.
    #[cfg(test)]
    let app = app.layer(middleware::from_fn(crate::openapi_contract::check));
    app.layer(NormalizePathLayer::trim_trailing_slash()).layer(
        TraceLayer::new_for_http()
            .make_span_with(|request: &Request| {
                tracing::span!(
                    Level::INFO,
                    "http.request",
                    method = %request.method(),
                    path = %request.uri().path(),
                )
            })
            .on_response(
                |response: &Response, latency: Duration, _span: &tracing::Span| {
                    let status = response.status();
                    let latency_ms = latency.as_millis() as u64;
                    if status.is_server_error() || latency_ms >= 1_000 {
                        tracing::warn!(
                            status = status.as_u16(),
                            latency_ms,
                            "request completed with elevated latency or server error"
                        );
                    } else if latency_ms >= 250 {
                        tracing::info!(status = status.as_u16(), latency_ms, "request completed");
                    }
                },
            )
            .on_failure(
                |failure: ServerErrorsFailureClass, latency: Duration, _span: &tracing::Span| {
                    tracing::error!(
                        failure = ?failure,
                        latency_ms = latency.as_millis() as u64,
                        "request failed"
                    );
                },
            ),
    )
}

/// Content-Security-Policy of the web app's document.
///
/// Scripts, styles, fonts, connections, workers and the manifest come only
/// from this origin; no plugin, object or frame content is allowed, and no
/// other site may frame the app. Images are same-origin too: covers, store
/// icons, plugin logos, Gravatars and sign-in provider pictures all come
/// through the cover service (`/api/covers/…`, `/api/stores/…/icon`,
/// `/api/avatars/gravatar/…`), so the browser contacts no other site.
/// `style-src` keeps `'unsafe-inline'` for style attributes in sanitized
/// store blurbs.
pub(crate) const SPA_CSP: &str = "default-src 'self'; script-src 'self'; \
     style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; \
     font-src 'self' data:; connect-src 'self'; worker-src 'self' blob:; \
     manifest-src 'self'; object-src 'none'; frame-src 'none'; \
     base-uri 'self'; form-action 'self'; frame-ancestors 'none'";

/// Adds [`SPA_CSP`] to HTML documents served from the web app's build
/// (its `index.html` on every app route, and any HTML file under `ui/dist`).
async fn spa_document_csp(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let is_html = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/html"));
    if is_html {
        res.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(SPA_CSP),
        );
    }
    res
}

/// The `text/plain` body of the 504 [`api_timeout_middleware`] returns.
pub(crate) const API_TIMEOUT_MESSAGE: &str = "request timed out";

/// Enforces per-path API deadlines and returns 504 when a `/api/` handler overruns its budget.
pub(crate) async fn api_timeout_middleware(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let method = req.method().to_string();
    // Keep `/health` free of the API timeout so liveness probes stay cheap.
    if path == "/health" || !path.starts_with("/api/") {
        return next.run(req).await;
    }
    // A host's streamed upload runs as long as its file takes; the handler
    // watches the job's fence instead. Only those PUTs: the commit is an
    // ordinary bounded request and keeps the normal deadline.
    if is_streamed_upload(req.method(), &path) {
        return next.run(req).await;
    }
    // Multi-store purchase / Audnexus detail work is intentionally slower than
    // the default control-plane budget; shelf cards batch these in the viewport.
    let budget = api_timeout_for_path(&path);
    let started = std::time::Instant::now();
    match timeout(budget, next.run(req)).await {
        Ok(res) => res,
        Err(_) => {
            tracing::warn!(
                %method,
                path,
                budget_secs = budget.as_secs(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "request timed out in api timeout middleware"
            );
            (StatusCode::GATEWAY_TIMEOUT, API_TIMEOUT_MESSAGE).into_response()
        }
    }
}

/// True for a host's streamed file upload, `PUT
/// /api/node/v1/uploads/{upload_id}/files/{index}`, the one request that
/// runs without the API deadline ([`api_timeout_middleware`]).
pub(crate) fn is_streamed_upload(method: &axum::http::Method, path: &str) -> bool {
    *method == axum::http::Method::PUT
        && path
            .strip_prefix(bookclerk_node_proto::UPLOADS_PATH)
            .and_then(|rest| rest.split_once('/'))
            .is_some_and(|(_, rest)| rest.starts_with("files/"))
}

/// Selects the request deadline in seconds for a control-plane path (8s default; longer for discover
/// and joining or leaving a library).
fn api_timeout_for_path(path: &str) -> Duration {
    match path {
        "/api/discover/purchase-hints"
        | "/api/discover/purchase-hints/batch"
        | "/api/discover/title-meta"
        | "/api/discover/title-meta/batch"
        | "/api/discover/title-reviews" => Duration::from_secs(25),
        // Parallel store searches (~7s each) + optional page enrich (~3.5s).
        "/api/discover/search" => Duration::from_secs(16),
        // Leave library waits for the library to hear `node.leaving`
        // (`cluster_link::LEAVE_NOTICE_TIMEOUT`, through a fresh connection
        // when no client runs) before it records the leave.
        "/api/config/join/leave" => Duration::from_secs(30),
        // A join redeems with the library (at most
        // `cluster_link::join::REDEEM_TIMEOUT`), then waits for the new
        // link's client (`cluster_link::start_link_now`).
        // Their own bounded waits add up to 29 s; the rest is margin for the
        // database round trips around them.
        "/api/config/join/callback" | "/api/config/join/invite" => Duration::from_secs(45),
        // Reading a library dials it (connect and request timeouts in
        // `bookclerk_node_client::endpoint`), so an unreachable one is
        // answered as `unreachable` rather than cut off.
        "/api/config/join/inspect" | "/api/config/join/start" => Duration::from_secs(45),
        // Adding or disconnecting store connections calls each library
        // once, concurrently, bounded by
        // `cluster_link::connections::LIBRARY_CALL_TIMEOUT` (12 s); the
        // rest is margin for the database round trips around it.
        "/api/config/join/connections" | "/api/config/join/connections/disconnect" => {
            Duration::from_secs(20)
        }
        _ => Duration::from_secs(8),
    }
}

/// Resolve the Vite build output directory for the GUI.
///
/// Prefer `BOOKCLERK_UI_DIST`, then paths beside the running binary (desktop
/// launches often have cwd=`$HOME`), then cwd-relative / compile-time paths.
#[must_use]
pub fn resolve_ui_dist() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("BOOKCLERK_UI_DIST") {
        let path = PathBuf::from(v.trim());
        if path.is_dir() {
            return Some(path.canonicalize().unwrap_or(path));
        }
        tracing::warn!(
            path = %path.display(),
            "BOOKCLERK_UI_DIST is set but not a directory"
        );
    }

    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.extend([
                dir.join("ui-dist"),
                dir.join("ui/dist"),
                dir.join("../ui/dist"),
                dir.join("../../ui/dist"),
            ]);
        }
    }
    candidates.extend([
        PathBuf::from("ui/dist"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/dist"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ui-dist"),
    ]);

    candidates.into_iter().find_map(|p| {
        p.is_dir()
            .then(|| p.canonicalize().unwrap_or(p))
            .filter(|p| p.is_dir())
    })
}

/// Serves `GET /health` with a static `ok` payload for liveness probes.
#[utoipa::path(
    get,
    path = "/health",
    responses(
        (status = 200, description = "The daemon is serving HTTP.", body = HealthResponse),
    ),
)]
async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

/// Build operator auth runtime from config + library DB (env / encrypted_secrets).
pub async fn build_operator_auth(
    config: &Config,
    library: &LibraryStore,
) -> anyhow::Result<OperatorAuthState> {
    let auth_cfg = &config.daemon.auth;
    if !auth_cfg.enabled {
        tracing::warn!("daemon.auth.enabled=false — HTTP API is unauthenticated");
        return Ok(OperatorAuthState::new(
            String::new(),
            auth_cfg.session_ttl_hours,
            false,
            auth_cfg.login_max_failures,
            auth_cfg.login_lockout_secs,
        ));
    }
    let (token, how) =
        bookclerk_library::read_or_create_operator_token(config, library.db()).await?;
    tracing::info!(?how, "resolved operator API token");
    Ok(OperatorAuthState::new(
        token,
        auth_cfg.session_ttl_hours,
        true,
        auth_cfg.login_max_failures,
        auth_cfg.login_lockout_secs,
    ))
}

/// Start integration watchers with claim-ticket minting (shared by startup + reload).
///
/// Awaits `start_all` on a registry snapshot so a delayed task cannot race a later
/// reload and double-start the live integrations.
pub async fn start_integration_watchers(state: &AppState) {
    let library_for_tickets = state.library.clone();
    let config_for_tickets = state.config.clone();
    let registry = state.integrations.read().await.clone();
    let ctx = bookclerk_integrations::IntegrationContext {
        on_external_user: Some(Arc::new(move |user| {
            let library_for_tickets = library_for_tickets.clone();
            let config_for_tickets = config_for_tickets.clone();
            tokio::spawn(async move {
                let cfg = config_for_tickets.read().await;
                let lib = library_for_tickets.read().await.clone();
                match bookclerk_integrations::mint_for_external_user(
                    &lib,
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
    state
        .deployment_runtime()
        .set_integration_context(ctx.clone());
    if let Err(err) = registry.start_all(ctx).await {
        tracing::warn!(%err, "integration start_all reported errors");
    }
}

/// Fingerprint of host-owned database connect settings (occupancy + URLs).
fn database_connect_fingerprint(cfg: &Config) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}",
        cfg.database.plugin,
        cfg.database.postgres.url.as_deref().unwrap_or(""),
        cfg.database
            .postgres
            .url_file
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        cfg.database.d1.api_base,
        cfg.database.d1.account_id,
        cfg.database.d1.database_id
    )
}

/// Reload `config.toml` from disk and publish a complete candidate runtime.
///
/// Auth is always rebuilt and swapped **before** listen rebind notification so a
/// public listener never outruns the middleware that enforces authentication.
pub async fn reload_daemon_config(state: &AppState) -> anyhow::Result<String> {
    let _reload_guard = state.reload_lock.lock().await;
    reload_daemon_config_held(state).await
}

/// Like [`reload_daemon_config`] when the caller already holds `reload_lock`.
///
/// Guests are not touched here: the deployment reconciler owns every source,
/// integration, and destination this process runs. Reload re-enrolls local
/// plugins (a newly installed plugin gets its instance and deployment row),
/// republishes auth and config, and then runs one reconcile pass so the
/// enrolled deployments come up without waiting for the next tick. A
/// database connect change retires every guest so the pass respawns them
/// against the new library.
pub(crate) async fn reload_daemon_config_held(state: &AppState) -> anyhow::Result<String> {
    let (files_dir, config_path, old_listen, old_db_fingerprint, old_auth_enabled, old_token) = {
        let cfg = state.config.read().await;
        let auth = state.auth.read().await;
        (
            cfg.paths().files_dir.clone(),
            cfg.paths().config_file.clone(),
            cfg.daemon.listen.clone(),
            database_connect_fingerprint(&cfg),
            auth.enabled,
            auth.token.clone(),
        )
    };

    let new_cfg = Config::load(Some(files_dir.clone()), Some(config_path.clone()))?;
    validate_daemon_listen(&new_cfg)?;
    new_cfg.warn_unsupported_options();

    // Build the full candidate before mutating live state.
    let db_plugin_changed = old_db_fingerprint != database_connect_fingerprint(&new_cfg);

    let _job_runtime = if db_plugin_changed {
        Some(
            timeout(Duration::from_secs(60), state.job_runtime.write())
                .await
                .map_err(|_| {
                    anyhow::anyhow!("timed out waiting for in-flight jobs before database swap")
                })?,
        )
    } else {
        None
    };
    let (candidate_db_registry, candidate_library) = if db_plugin_changed {
        let registry = bookclerk_plugin_host::load_external_database(&new_cfg).await?;
        let library = bookclerk_plugin_host::open_library_store(&new_cfg, &registry).await?;
        (Some(registry), Some(library))
    } else {
        (None, None)
    };

    let library_for_auth = match &candidate_library {
        Some(lib) => lib.clone(),
        None => state.library_snapshot().await,
    };
    let bookclerk_plugin_host::NodeEnrollment {
        config: mut new_cfg,
        session: control_plane,
        enrolled,
    } = bookclerk_plugin_host::enroll_node(&new_cfg, &library_for_auth).await?;
    tracing::info!(
        node_id = %control_plane.node.node_id,
        enrolled = enrolled.len(),
        "re-enrolled local plugins on config reload"
    );

    let candidate_auth = build_operator_auth(&new_cfg, &library_for_auth).await?;
    // Defense in depth: never publish a non-loopback listen with auth disabled
    // in the *runtime* object (validate_daemon_listen already checked config).
    validate_daemon_listen_against_auth(&new_cfg, candidate_auth.enabled)?;
    // The node API's TLS listener stays bound from startup whatever the new
    // config says, and it serves the web app too.
    if !candidate_auth.enabled {
        if let Some(bound) = state.node_api_tls_bound.get() {
            if !bound.ip().is_loopback() {
                anyhow::bail!(
                    "daemon.auth.enabled=false is unsafe while the node API's TLS listener is bound beyond loopback ({bound}); restart the daemon to close it"
                );
            }
        }
    }

    let token_changed = candidate_auth.enabled != old_auth_enabled
        || (candidate_auth.enabled && candidate_auth.token != old_token);
    let listen_changed = old_listen != new_cfg.daemon.listen;

    // Read the candidate events document before any live swap. A database
    // error here leaves library, auth, and the media pool on the previous
    // runtime.
    let fresh = crate::config_authority::load_events_publication(&library_for_auth).await?;
    let fresh_output = crate::config_authority::load_output_publication(&library_for_auth).await?;

    // Fail closed before publishing when the new listen set cannot bind at all
    // (ports we already hold are skipped — those need a post-rebind rollback).
    if listen_changed {
        preflight_listen_bind(&old_listen, &new_cfg.daemon.listen).await?;
    }

    // Process-global media pool: only after preflight so a failed reload cannot
    // leave workers on the candidate `[media]` while AppState stays on the old one.
    bookclerk_media::init_pool_from_config(&new_cfg.media);

    // Publish.
    {
        if let (Some(registry), Some(library)) = (candidate_db_registry, candidate_library) {
            // Every guest was opened against the previous database; retire
            // them so the reconcile pass below respawns each present
            // deployment with bindings resolved from the new one.
            let runtime = state.deployment_runtime();
            let tracked =
                bookclerk_plugin_host::DeploymentRuntime::tracked_instance_ids(runtime.as_ref())
                    .await;
            for instance_id in tracked {
                runtime.retire_plugin_instance(&instance_id).await;
            }
            *state.database_registry.write().await = registry;
            *state.library.write().await = library;
        }

        // Auth before config / listen notify — security boundary.
        {
            let mut auth_guard = state.auth.write().await;
            let previous = auth_guard.clone();
            let mut next = candidate_auth;
            next.install_token_grace_from(&previous, auth::OPERATOR_TOKEN_GRACE);
            next.take_session_state_from(&previous).await;
            *auth_guard = Arc::new(next);
        }
        {
            let mut config = state.config.write().await;
            crate::config_authority::finish_reload_events(
                &config,
                &mut new_cfg,
                &fresh,
                db_plugin_changed,
            );
            crate::config_authority::finish_reload_output(
                &config,
                &mut new_cfg,
                &fresh_output,
                db_plugin_changed,
            );
            *config = new_cfg.clone();
        }
    }

    if db_plugin_changed {
        // Host clients hold the previous store's links and keys; the
        // cluster-link supervisor stops them and dials the new store's
        // active links.
        state.library_swaps.send_modify(|swaps| *swaps += 1);
    }

    bookclerk_plugin_host::reconcile_host_overlay_authority(&new_cfg);

    // Bring the enrolled deployment set up now rather than on the next tick.
    if let Err(err) = crate::config_authority::reconcile_deployments(state).await {
        tracing::warn!(error = %err, "deployment reconcile after reload failed");
    }

    // Start watchers for the integration set (awaited — no untracked race).
    start_integration_watchers(state).await;
    crate::event_worker::upsert_event_subscriber_catalog(state).await;
    state.job_notify.notify_waiters();

    if let Err(err) = crate::oidc::sync_plugin_oidc_clients(state).await {
        tracing::warn!(error = %err, "failed to sync plugin OIDC clients after config reload");
    }

    refresh_tray_after_reload(state, &new_cfg).await;

    let mut detail = format!(
        "reloaded {} (master.key wrap={})",
        config_path.display(),
        new_cfg.auth_password().is_some()
    );
    if db_plugin_changed {
        detail.push_str("; database connect target updated");
    }
    if token_changed {
        detail.push_str("; operator auth runtime updated");
    }
    detail.push_str(&format!(
        "; sources={} integrations={}",
        state.sources.read().await.all().len(),
        state.integrations.read().await.all().len()
    ));
    if listen_changed {
        detail.push_str(&format!(
            "; rebinding HTTP listeners `{}` → `{}`",
            old_listen.join_comma(),
            new_cfg.daemon.listen.join_comma()
        ));
        state.listen_reload.notify_waiters();
    }
    Ok(detail)
}

/// Validate listen, and the node API's TLS listener (which serves the web
/// app too), against the effective runtime auth flag (not only TOML).
///
/// # Errors
///
/// Returns an error when `daemon.listen` does not parse or is empty, or when
/// auth is off and either listener is reachable beyond loopback.
pub fn validate_daemon_listen_against_auth(
    config: &Config,
    auth_enabled: bool,
) -> anyhow::Result<()> {
    let addrs = config
        .daemon
        .listen
        .socket_addrs()
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    if addrs.is_empty() {
        anyhow::bail!("daemon.listen must not be empty");
    }
    if !auth_enabled {
        for addr in &addrs {
            if !addr.ip().is_loopback() {
                anyhow::bail!(
                    "daemon.auth.enabled=false is unsafe when listen is not loopback ({})",
                    config.daemon.listen.join_comma()
                );
            }
        }
        // The node API's own TLS listener serves the whole web app too (a
        // branch's owner signs in there), so it is held to the same rule.
        let node_api = &config.daemon.node_api;
        if matches!(
            node_api.mode,
            bookclerk_config::NodeApiMode::Acme | bookclerk_config::NodeApiMode::SelfSigned
        ) {
            if let Some(tls) = node_api
                .tls_listen
                .as_deref()
                .and_then(|addr| addr.trim().parse::<std::net::SocketAddr>().ok())
                .filter(|addr| !addr.ip().is_loopback())
            {
                anyhow::bail!(
                    "daemon.auth.enabled=false is unsafe when daemon.node_api.tls_listen is not loopback ({tls}): that listener serves the web app too"
                );
            }
        }
    }
    Ok(())
}

/// Pushes the reloaded listen set and auth flags into the optional tray companion.
async fn refresh_tray_after_reload(state: &AppState, config: &Config) {
    let tray = state.tray.read().await.clone();
    let Some(tray) = tray else {
        return;
    };
    let auth = state.auth_snapshot().await;
    crate::tray_companion::update_tray_after_reload(&tray, config, &auth);
}

/// Parse `daemon.listen` and reject unsafe auth/listen combinations.
pub fn validate_daemon_listen(config: &Config) -> anyhow::Result<()> {
    validate_daemon_listen_against_auth(config, config.daemon.auth.enabled)
}

/// Bind every configured listen address; skip failures unless none succeed.
pub async fn bind_listen_addrs(
    listen: &ListenAddrs,
) -> anyhow::Result<Vec<tokio::net::TcpListener>> {
    let addrs = listen
        .socket_addrs()
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    let mut listeners = Vec::new();
    let mut errors = Vec::new();
    for addr in addrs {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => listeners.push(listener),
            Err(err) => {
                tracing::warn!(
                    %addr,
                    error = %err,
                    "failed to bind daemon.listen address; skipping"
                );
                errors.push(format!("{addr}: {err}"));
            }
        }
    }
    if listeners.is_empty() {
        anyhow::bail!(
            "could not bind any daemon.listen address ({})",
            errors.join("; ")
        );
    }
    Ok(listeners)
}

/// Preflight-bind addresses in `new` that are not already held by `old`.
///
/// Addresses that share a port with a current listener cannot be probed while
/// the old sockets are live; those transitions rely on post-rebind rollback via
/// [`AppState::last_bound_listen`].
pub async fn preflight_listen_bind(old: &ListenAddrs, new: &ListenAddrs) -> anyhow::Result<()> {
    let old_addrs = old.socket_addrs().map_err(|err| anyhow::anyhow!("{err}"))?;
    let new_addrs = new.socket_addrs().map_err(|err| anyhow::anyhow!("{err}"))?;
    if new_addrs.is_empty() {
        anyhow::bail!("daemon.listen must not be empty");
    }

    let mut can_succeed = false;
    let mut errors = Vec::new();
    let mut held = Vec::new();
    for addr in &new_addrs {
        let conflicts_with_current = old_addrs.iter().any(|old_addr| {
            old_addr == addr
                || (old_addr.port() == addr.port()
                    && old_addr.ip().is_ipv4() == addr.ip().is_ipv4())
        });
        if conflicts_with_current {
            // Same port still held by the live listeners; assume rebind can reuse it.
            can_succeed = true;
            continue;
        }
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                can_succeed = true;
                held.push(listener);
            }
            Err(err) => errors.push(format!("{addr}: {err}")),
        }
    }
    drop(held);
    if !can_succeed {
        anyhow::bail!(
            "daemon.listen preflight bind failed; reload aborted ({})",
            errors.join("; ")
        );
    }
    Ok(())
}

/// After a failed rebind, restore the last successfully bound listen set in live config.
pub async fn revert_listen_after_bind_failure(state: &AppState) -> bool {
    let Some(previous) = state.last_bound_listen.read().await.clone() else {
        return false;
    };
    let mut cfg = state.config.write().await;
    if cfg.daemon.listen == previous {
        return false;
    }
    tracing::error!(
        attempted = %cfg.daemon.listen.join_comma(),
        restored = %previous.join_comma(),
        "listen rebind failed; reverting daemon.listen to last bound addresses"
    );
    cfg.daemon.listen = previous;
    true
}

/// Which store owns a Settings key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SettingsAuthority {
    /// `config.toml` (bootstrap and still-transitional tables).
    File,
    /// The `core.events` cluster document.
    Events,
    /// The `core.output` cluster document.
    Output,
    /// The `core.cache` cluster document.
    Cache,
}

/// Authority that owns `key`.
fn settings_authority(key: &str) -> SettingsAuthority {
    if key.starts_with("events.") {
        SettingsAuthority::Events
    } else if bookclerk_library::control_plane::is_output_setting_key(key) {
        SettingsAuthority::Output
    } else if bookclerk_library::control_plane::is_cache_setting_key(key) {
        SettingsAuthority::Cache
    } else {
        SettingsAuthority::File
    }
}

/// True when `key` is a Settings-editable dotted path (global knobs or `kind.id.field`).
fn allowed_setting_key(key: &str) -> bool {
    if matches!(
        key,
        "daemon.listen"
            | "daemon.auth.enabled"
            | "daemon.auth.require_second_factor"
            | "library.auto_acquire"
            | "library.scan_interval_minutes"
            | "jobs.max_pending"
            | "jobs.lease_seconds"
            | "jobs.max_attempts"
            | "jobs.retention_days"
            | "jobs.temp_quota_bytes"
            | "jobs.concurrency.network"
            | "events.retention_days"
            | "events.dead_letter_retention_days"
            | "events.concurrency"
            | "database.plugin"
            | "plugins.isolation"
            | "media.isolation"
            | "plugins.jail.memory_mib"
            | "plugins.jail.cpu_rate_percent"
            | "plugins.jail.cpu_cores"
            | "plugins.jail.extra_processes"
    ) {
        return true;
    }
    if bookclerk_library::control_plane::OUTPUT_SETTING_KEYS.contains(&key)
        || bookclerk_library::control_plane::is_cache_setting_key(key)
    {
        return true;
    }

    fn valid_scoped_key(key: &str, prefix: &str) -> bool {
        let Some(rest) = key.strip_prefix(prefix) else {
            return false;
        };
        let mut parts = rest.split('.');
        let Some(id) = parts.next() else {
            return false;
        };
        let Some(field) = parts.next() else {
            return false;
        };
        !id.is_empty() && !field.is_empty() && parts.next().is_none()
    }

    // Plugin settings live on plugin instances
    // (`PUT /api/config/plugin-instances/{id}/config`); only the bootstrap
    // database adapters keep a `config.toml` table.
    valid_scoped_key(key, "database.")
}

/// Validates and canonicalizes a Settings value; returns an error string for invalid input.
fn normalize_setting_value(key: &str, value: &str) -> Result<String, String> {
    if bookclerk_library::control_plane::is_cache_setting_key(key) {
        let mut body = bookclerk_library::control_plane::CacheSettingsV1::default();
        body.set(key, value).map_err(|err| err.to_string())?;
        return body
            .get(key)
            .ok_or_else(|| format!("unknown cache setting: {key}"));
    }
    match key {
        "library.scan_interval_minutes" => value
            .parse::<u64>()
            .map(|_| value.to_string())
            .map_err(|_| "library.scan_interval_minutes must be a non-negative integer".into()),
        "jobs.max_pending" | "jobs.max_attempts" | "jobs.concurrency.network" => value
            .parse::<u32>()
            .map(|n| n.max(1).to_string())
            .map_err(|_| format!("{key} must be a positive integer")),
        "events.concurrency" => {
            let n = value
                .parse::<u32>()
                .map_err(|_| format!("{key} must be an integer"))?;
            if !(1..=bookclerk_library::control_plane::MAX_EVENTS_CONCURRENCY).contains(&n) {
                return Err(format!(
                    "{key} must be 1..={}",
                    bookclerk_library::control_plane::MAX_EVENTS_CONCURRENCY
                ));
            }
            Ok(n.to_string())
        }
        "events.retention_days" | "events.dead_letter_retention_days" => {
            let n = value
                .parse::<u64>()
                .map_err(|_| format!("{key} must be an integer"))?;
            if !(1..=bookclerk_library::control_plane::MAX_EVENTS_RETENTION_DAYS).contains(&n) {
                return Err(format!(
                    "{key} must be 1..={}",
                    bookclerk_library::control_plane::MAX_EVENTS_RETENTION_DAYS
                ));
            }
            Ok(n.to_string())
        }
        "jobs.lease_seconds" | "jobs.retention_days" | "jobs.temp_quota_bytes" => value
            .parse::<u64>()
            .map(|n| n.to_string())
            .map_err(|_| format!("{key} must be a non-negative integer")),
        "library.auto_acquire" | "daemon.auth.enabled" | "daemon.auth.require_second_factor" => {
            match value.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" | "0" | "false" | "no" | "off" => Ok(value.to_string()),
                _ => Err(format!("{key} must be a boolean value")),
            }
        }
        "daemon.listen" => {
            // Validate listen list before writing config.toml (supports multi-bind).
            bookclerk_config::ListenAddrs::parse_list(value).map(|addrs| addrs.join_comma())
        }
        "plugins.isolation" | "media.isolation" => bookclerk_config::Isolation::parse(value)
            .map(|mode| mode.as_str().to_string())
            .ok_or_else(|| format!("{key} must be required, best-effort, or off")),
        "plugins.jail.memory_mib" => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Ok(String::new());
            }
            trimmed
                .parse::<u64>()
                .map(|n| n.to_string())
                .map_err(|_| "plugins.jail.memory_mib must be a non-negative integer".into())
        }
        "plugins.jail.cpu_rate_percent" | "plugins.jail.cpu_cores" => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                // Empty resets to the platform default (0.80 cores).
                return Ok(format_cpu_cores(PLUGIN_JAIL_CPU_CORES_DEFAULT));
            }
            // UI sends cores (e.g. "0.80"); integer percent still accepted.
            let cores = if trimmed.contains('.') {
                trimmed
                    .parse::<f64>()
                    .map_err(|_| "plugins.jail.cpu_cores must be a number".to_string())?
            } else {
                let percent = trimmed
                    .parse::<u32>()
                    .map_err(|_| "plugins.jail.cpu_cores must be a number".to_string())?;
                percent_to_cores(percent)
            };
            let clamped = effective_cpu_cores(Some(cores));
            Ok(format_cpu_cores(clamped))
        }
        "plugins.jail.extra_processes" => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Ok(PLUGIN_JAIL_EXTRA_PROCESSES_DEFAULT.to_string());
            }
            trimmed
                .parse::<u32>()
                .map(|n| n.min(PLUGIN_JAIL_EXTRA_PROCESSES_MAX).to_string())
                .map_err(|_| "plugins.jail.extra_processes must be a non-negative integer".into())
        }
        _ if (key.starts_with("sources.")
            || key.starts_with("integrations.")
            || key.starts_with("output."))
            && key.ends_with(".enabled") =>
        {
            match value.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => Ok("true".into()),
                "0" | "false" | "no" | "off" => Ok("false".into()),
                _ => Err(format!("{key} must be a boolean value")),
            }
        }
        _ if key.starts_with("database.") && key.ends_with(".enabled") => {
            match value.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => Ok("true".into()),
                "0" | "false" | "no" | "off" => Ok("false".into()),
                _ => Err(format!("{key} must be a boolean value")),
            }
        }
        _ => Ok(value.trim().to_string()),
    }
}

/// Adds every `core.output` key to a Settings map.
fn insert_output_settings(
    settings: &mut std::collections::BTreeMap<String, String>,
    body: &bookclerk_library::control_plane::OutputSettingsV1,
) {
    for key in bookclerk_library::control_plane::OUTPUT_SETTING_KEYS {
        if let Some(value) = body.get(key) {
            settings.insert((*key).to_string(), value);
        }
    }
}

/// Builds the file-backed Settings map from live `Config` (listen, isolation, jail, sources).
fn current_settings_snapshot(config: &Config) -> std::collections::BTreeMap<String, String> {
    let mut settings = std::collections::BTreeMap::new();
    settings.insert("daemon.listen".into(), config.daemon.listen.join_comma());
    settings.insert(
        "daemon.auth.enabled".into(),
        config.daemon.auth.enabled.to_string(),
    );
    settings.insert(
        "daemon.auth.require_second_factor".into(),
        config.daemon.auth.require_second_factor.to_string(),
    );
    settings.insert(
        "library.auto_acquire".into(),
        config.library.auto_acquire.to_string(),
    );
    settings.insert(
        "library.scan_interval_minutes".into(),
        config.library.scan_interval_minutes.to_string(),
    );
    settings.insert(
        "jobs.max_pending".into(),
        config.jobs.max_pending.to_string(),
    );
    settings.insert(
        "jobs.lease_seconds".into(),
        config.jobs.lease_seconds.to_string(),
    );
    settings.insert(
        "jobs.max_attempts".into(),
        config.jobs.max_attempts.to_string(),
    );
    settings.insert(
        "jobs.retention_days".into(),
        config.jobs.retention_days.to_string(),
    );
    settings.insert(
        "jobs.temp_quota_bytes".into(),
        config.jobs.temp_quota_bytes.to_string(),
    );
    settings.insert(
        "jobs.concurrency.network".into(),
        config.jobs.concurrency.network.to_string(),
    );
    settings.insert(
        "events.retention_days".into(),
        config.events.retention_days.to_string(),
    );
    settings.insert(
        "events.dead_letter_retention_days".into(),
        config.events.dead_letter_retention_days.to_string(),
    );
    settings.insert(
        "events.concurrency".into(),
        config.events.concurrency.to_string(),
    );
    settings.insert(
        "plugins.isolation".into(),
        config.plugins.isolation.as_str().to_string(),
    );
    settings.insert(
        "media.isolation".into(),
        config.media.isolation.as_str().to_string(),
    );
    settings.insert(
        "plugins.jail.memory_mib".into(),
        config
            .plugins
            .jail
            .memory_mib
            .map(|n| n.to_string())
            .unwrap_or_default(),
    );
    settings.insert(
        "plugins.jail.cpu_cores".into(),
        format_cpu_cores(percent_to_cores(
            config
                .plugins
                .jail
                .cpu_rate_percent
                .unwrap_or(PLUGIN_JAIL_CPU_RATE_DEFAULT),
        )),
    );
    settings.insert(
        "plugins.jail.extra_processes".into(),
        config
            .plugins
            .jail
            .extra_processes
            .unwrap_or(PLUGIN_JAIL_EXTRA_PROCESSES_DEFAULT)
            .to_string(),
    );
    settings
}

/// Turns a snake_case setting key fragment into Title Case UI copy.
fn setting_label(key: &str) -> String {
    key.replace('_', " ")
        .split_whitespace()
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Wire label for a handler family (`source`, `integration`, `output`, `database`).
fn plugin_family_label(family: bookclerk_plugin_host::PluginFamily) -> &'static str {
    family.as_str()
}

/// Resolve Settings `logo` from `plugin.toml` (`https://…` or host-served embed path).
fn settings_logo_from_manifest(plugin: &bookclerk_plugin_host::DiscoveredPlugin) -> Option<String> {
    let kind = match plugin.manifest.logo_kind() {
        Ok(k) => k?,
        Err(_) => return None,
    };
    match kind {
        bookclerk_plugin_host::LogoKind::RemoteUrl(url) => Some(url),
        bookclerk_plugin_host::LogoKind::EmbeddedPath(_) => {
            Some(bookclerk_plugin_host::embedded_logo_api_path(
                plugin.manifest.primary_family().as_str(),
                &plugin.manifest.id,
            ))
        }
    }
}

/// Returns a trimmed logo URL, or `None` when the string is empty.
fn non_empty_logo(url: &str) -> Option<String> {
    let t = url.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Builds a free-form Settings option with no dropdown choices.
fn plugin_setting_option(
    key: String,
    label: impl Into<String>,
    value: impl Into<String>,
    value_type: &'static str,
) -> PluginSettingOption {
    PluginSettingOption {
        key,
        label: label.into(),
        value: value.into(),
        value_type: value_type.into(),
        choices: None,
        secret: false,
        secret_set: None,
    }
}

/// Builds one dropdown choice for a Settings option.
fn plugin_setting_choice(
    value: impl Into<String>,
    label: impl Into<String>,
) -> PluginSettingChoice {
    PluginSettingChoice {
        value: value.into(),
        label: label.into(),
    }
}

/// Builds a Settings option that exposes a dropdown when `choices` is non-empty.
fn plugin_setting_option_with_choices(
    key: String,
    label: impl Into<String>,
    value: impl Into<String>,
    value_type: &'static str,
    choices: Vec<PluginSettingChoice>,
) -> PluginSettingOption {
    PluginSettingOption {
        key,
        label: label.into(),
        value: value.into(),
        value_type: value_type.into(),
        choices: if choices.is_empty() {
            None
        } else {
            Some(choices)
        },
        secret: false,
        secret_set: None,
    }
}

/// Typed `[database.<id>]` options for the first-party adapters.
///
/// Database adapters are the bootstrap exception: their connect settings
/// stay in `config.toml`. Every other plugin declares `[[settings]]` and is
/// edited through its plugin instance.
fn built_in_plugin_settings(config: &Config, id: &str) -> Vec<PluginSettingOption> {
    let prefix = format!("database.{id}");
    match id {
        "sqlite" => {
            let cfg = &config.database.sqlite;
            vec![plugin_setting_option(
                format!("{prefix}.path"),
                "Path",
                cfg.path
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                "string",
            )]
        }
        "d1" => {
            let cfg = &config.database.d1;
            vec![
                plugin_setting_option(
                    format!("{prefix}.account_id"),
                    "Account ID",
                    cfg.account_id.clone(),
                    "string",
                ),
                plugin_setting_option(
                    format!("{prefix}.database_id"),
                    "Database ID",
                    cfg.database_id.clone(),
                    "string",
                ),
                plugin_setting_option(
                    format!("{prefix}.api_base"),
                    "API Base",
                    cfg.api_base.clone(),
                    "string",
                ),
            ]
        }
        "postgres" => {
            let cfg = &config.database.postgres;
            vec![
                plugin_setting_option(
                    format!("{prefix}.url"),
                    "URL",
                    cfg.url.clone().unwrap_or_default(),
                    "string",
                ),
                plugin_setting_option(
                    format!("{prefix}.url_file"),
                    "URL File",
                    cfg.url_file
                        .as_ref()
                        .map(|path| path.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    "string",
                ),
            ]
        }
        _ => Vec::new(),
    }
}

/// Settings group for a bootstrap database adapter (`database.<id>.*`).
fn build_database_settings_group(
    config: &Config,
    id: &str,
    table: toml::Table,
) -> PluginSettingsGroup {
    let prefix = format!("database.{id}");
    let mut options = Vec::new();
    let mut seen_keys = std::collections::BTreeSet::new();

    let enabled_key = format!("{prefix}.enabled");
    seen_keys.insert(enabled_key.clone());
    options.push(plugin_setting_option(
        enabled_key,
        "Enabled",
        bookclerk_plugin_host::occupancy_matches_alias(config.database.plugin.trim(), id)
            .to_string(),
        "boolean",
    ));

    for option in built_in_plugin_settings(config, id) {
        seen_keys.insert(option.key.clone());
        options.push(option);
    }

    for (key, entry) in &table {
        if key == "enabled" {
            continue;
        }
        let full_key = format!("{prefix}.{key}");
        if seen_keys.contains(&full_key) {
            continue;
        }
        let value_text = entry
            .as_str()
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| entry.to_string());
        let value_type = if entry.as_bool().is_some() {
            "boolean"
        } else if entry.as_integer().is_some() || entry.as_float().is_some() {
            "number"
        } else {
            "string"
        };
        options.push(plugin_setting_option(
            full_key,
            setting_label(key),
            value_text,
            value_type,
        ));
    }

    PluginSettingsGroup {
        id: id.to_string(),
        family: plugin_family_label(bookclerk_plugin_host::PluginFamily::Database).to_string(),
        plugin_key: None,
        provenance: None,
        logo: None,
        settings: options,
        plugin_instance_id: None,
        display_name: None,
        is_default: false,
        config_revision: None,
        deployment: None,
        placement: None,
        present_on: Vec::new(),
    }
}

/// Settings groups: one per plugin instance from the control plane, plus the
/// bootstrap database adapters discovered on this node.
async fn plugin_settings_snapshot(
    config: &Config,
    library: &bookclerk_library::LibraryStore,
    node_id: &str,
    discovered_plugins: &[bookclerk_plugin_host::DiscoveredPlugin],
) -> Vec<PluginSettingsGroup> {
    let mut groups = Vec::new();
    for plugin in discovered_plugins {
        if !plugin
            .manifest
            .families()
            .contains(&bookclerk_plugin_host::PluginFamily::Database)
        {
            continue;
        }
        let table = bookclerk_plugin_host::settings_table_for(
            config,
            plugin,
            bookclerk_plugin_host::PluginFamily::Database,
        );
        let mut group = build_database_settings_group(config, &plugin.manifest.id, table);
        group.logo = settings_logo_from_manifest(plugin);
        let spec = config.database.plugin.trim();
        let enabled =
            !spec.is_empty() && bookclerk_plugin_host::plugin_matches_occupancy(plugin, spec);
        if let Some(option) = group
            .settings
            .iter_mut()
            .find(|option| option.key == format!("database.{}.enabled", plugin.alias()))
        {
            option.value = enabled.to_string();
        }
        group.plugin_key = Some(plugin.plugin_key().canonical().to_string());
        group.provenance = Some(plugin.identity.provenance.to_string());
        groups.push(group);
    }

    let views = match crate::config_authority::list_plugin_instance_views(library, node_id).await {
        Ok(views) => views,
        Err(err) => {
            tracing::warn!(error = %err, "plugin instances unavailable for settings");
            Vec::new()
        }
    };
    for view in &views {
        let plugin = discovered_plugins
            .iter()
            .find(|plugin| plugin.plugin_key().canonical() == view.plugin_key);
        groups.push(instance_settings_group(view, plugin, node_id));
    }
    groups.sort_by(|a, b| {
        (a.family.as_str(), a.id.as_str(), &a.display_name).cmp(&(
            b.family.as_str(),
            b.id.as_str(),
            &b.display_name,
        ))
    });
    groups
}

/// Settings group for one plugin instance, rendered from its pinned schema.
///
/// Option keys are the bare setting keys; the group's `plugin_instance_id`
/// tells the client to write them with the plugin-instance config route.
fn instance_settings_group(
    view: &crate::config_authority::PluginInstanceView,
    plugin: Option<&bookclerk_plugin_host::DiscoveredPlugin>,
    node_id: &str,
) -> PluginSettingsGroup {
    use bookclerk_library::control_plane::SettingValue;
    use bookclerk_plugin_abi::SettingKind;

    let stored = view.config.as_ref();
    let mut options = Vec::new();
    for spec in &view.schema {
        let value = stored
            .and_then(|config| config.settings.get(&spec.key))
            .map(|value| match value {
                SettingValue::Bool(flag) => flag.to_string(),
                SettingValue::Number(number) => number.to_string(),
                SettingValue::String(text) => text.clone(),
            })
            .or_else(|| {
                spec.default.as_ref().map(|default| match default {
                    serde_json::Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
            })
            .unwrap_or_default();
        let value_type = match spec.kind {
            SettingKind::Bool => "boolean",
            SettingKind::Integer => "number",
            SettingKind::String | SettingKind::Enum => "string",
            SettingKind::Secret => "secret",
        };
        let label = if spec.label.trim().is_empty() {
            setting_label(&spec.key)
        } else {
            spec.label.clone()
        };
        let mut option = if spec.kind == SettingKind::Enum {
            plugin_setting_option_with_choices(
                spec.key.clone(),
                label,
                value,
                value_type,
                spec.values
                    .iter()
                    .map(|choice| {
                        plugin_setting_choice(
                            choice.id.clone(),
                            if choice.label.trim().is_empty() {
                                choice.id.clone()
                            } else {
                                choice.label.clone()
                            },
                        )
                    })
                    .collect(),
            )
        } else {
            plugin_setting_option(spec.key.clone(), label, value, value_type)
        };
        if spec.kind == SettingKind::Secret {
            option.value = String::new();
            option.secret = true;
            option.secret_set = Some(
                stored.is_some_and(|config| config.secret_refs.iter().any(|r| r.key == spec.key)),
            );
        }
        options.push(option);
    }

    let family = plugin
        .map(|plugin| plugin.manifest.primary_family().as_str().to_string())
        .unwrap_or_else(|| "plugin".to_string());
    PluginSettingsGroup {
        id: if view.alias.is_empty() {
            view.plugin_key.clone()
        } else {
            view.alias.clone()
        },
        family,
        plugin_key: Some(view.plugin_key.clone()),
        provenance: plugin.map(|plugin| plugin.identity.provenance.to_string()),
        logo: plugin.and_then(settings_logo_from_manifest),
        settings: options,
        plugin_instance_id: Some(view.plugin_instance_id.clone()),
        display_name: (!view.display_name.is_empty()).then(|| view.display_name.clone()),
        is_default: view.is_default,
        config_revision: view.config.as_ref().map(|config| config.revision),
        deployment: view
            .deployments
            .iter()
            .find(|deployment| deployment.node_id == node_id)
            .cloned(),
        placement: view.placement.clone(),
        present_on: view
            .deployments
            .iter()
            .filter(|deployment| deployment.desired.runs())
            .map(|deployment| deployment.node_id.clone())
            .collect(),
    }
}

/// Discovers plugins on a blocking thread with a 2s timeout; returns empty on failure.
async fn discover_plugins_for_settings(
    config: &Config,
) -> Vec<bookclerk_plugin_host::DiscoveredPlugin> {
    const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(2);
    let cfg = config.clone();
    let task = tokio::task::spawn_blocking(move || bookclerk_plugin_host::discover_plugins(&cfg));

    match timeout(DISCOVERY_TIMEOUT, task).await {
        Ok(Ok(Ok(plugins))) => plugins,
        Ok(Ok(Err(err))) => {
            tracing::warn!(error = %err, "failed to discover plugins for settings");
            Vec::new()
        }
        Ok(Err(err)) => {
            tracing::warn!(error = %err, "settings plugin discovery task failed");
            Vec::new()
        }
        Err(_) => {
            tracing::warn!("settings plugin discovery timed out");
            Vec::new()
        }
    }
}

/// Applies `database.<id>.enabled` toggles so at most one backend remains selected.
fn apply_database_enable_updates(
    config: &mut Config,
    updates: &[(String, String)],
    discovered: &[bookclerk_plugin_host::DiscoveredPlugin],
) -> Result<(), String> {
    let mut enabled_targets = Vec::new();
    let mut disabled_targets = Vec::new();
    for (key, value) in updates {
        let Some(rest) = key.strip_prefix("database.") else {
            continue;
        };
        let Some(id) = rest.strip_suffix(".enabled") else {
            continue;
        };
        if id.is_empty() || id.contains('.') {
            continue;
        }
        match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => enabled_targets.push(id.to_string()),
            "0" | "false" | "no" | "off" => disabled_targets.push(id.to_string()),
            _ => {}
        }
    }
    enabled_targets.sort();
    enabled_targets.dedup();
    if enabled_targets.len() > 1 {
        return Err("only one database plugin can be enabled at a time".into());
    }
    if let Some(id) = enabled_targets.into_iter().next() {
        config.database.plugin = id;
        return Ok(());
    }
    // Unchecking the active backend with no replacement clears `database.plugin`
    // so the prior plugin does not stay selected after save.
    for id in disabled_targets {
        if bookclerk_plugin_host::occupancy_names_alias(&config.database.plugin, &id, discovered) {
            config.database.plugin.clear();
            break;
        }
    }
    Ok(())
}

/// Stamps PluginKey occupancy for newly enabled plugins, then upgrades unique aliases.
///
/// # Errors
///
/// Returns when an enable target cannot be resolved uniquely or occupancy
/// cannot be written (unmapped output family).
fn stamp_settings_occupancy(
    cfg: &mut Config,
    discovered: &[bookclerk_plugin_host::DiscoveredPlugin],
    enabling: &[String],
) -> Result<(), String> {
    for plugin_id in enabling {
        let plugin = bookclerk_plugin_host::resolve_plugin_ref(discovered, plugin_id)
            .map_err(|err| err.to_string())?;
        bookclerk_plugin_host::stamp_occupancy_plugin_key(cfg, plugin)
            .map_err(|err| err.to_string())?;
    }
    bookclerk_plugin_host::upgrade_unique_alias_occupancy(cfg, discovered);
    Ok(())
}

/// True for truthy Settings strings (`1`, `true`, `yes`, `on`).
fn setting_value_is_enabled(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Plugin ids newly flipped to enabled via `*.enabled` settings keys.
fn newly_enabled_plugin_ids(
    updates: &[(String, String)],
    previous: &std::collections::HashMap<&str, &PluginSettingOption>,
) -> Vec<String> {
    let mut ids = Vec::new();
    for (key, value) in updates {
        if !setting_value_is_enabled(value) {
            continue;
        }
        let parts: Vec<&str> = key.split('.').collect();
        if parts.len() != 3 || parts[2] != "enabled" {
            continue;
        }
        if parts[0] != "database" {
            continue;
        }
        let prev = previous
            .get(key.as_str())
            .map(|opt| opt.value.as_str())
            .unwrap_or("false");
        if setting_value_is_enabled(prev) {
            continue;
        }
        ids.push(parts[1].to_string());
    }
    ids.sort();
    ids.dedup();
    ids
}

/// Plugin ids that become the active database backend via this settings patch.
///
/// Covers both `database.<id>.enabled=true` and a direct `database.plugin=<id>`
/// override (the latter is allowlisted outside the editable-key set).
fn database_backends_requiring_grant(
    updates: &[(String, String)],
    current_database_plugin: &str,
) -> Vec<String> {
    let mut ids = Vec::new();
    for (key, value) in updates {
        if key == "database.plugin" {
            let id = value.trim();
            if id.is_empty() {
                continue;
            }
            if bookclerk_plugin_host::occupancy_matches_alias(current_database_plugin, id) {
                continue;
            }
            ids.push(id.to_string());
            continue;
        }
        let Some(rest) = key.strip_prefix("database.") else {
            continue;
        };
        let Some(id) = rest.strip_suffix(".enabled") else {
            continue;
        };
        if id.is_empty() || id.contains('.') || !setting_value_is_enabled(value) {
            continue;
        }
        if bookclerk_plugin_host::occupancy_matches_alias(current_database_plugin, id) {
            continue;
        }
        ids.push(id.to_string());
    }
    ids.sort();
    ids.dedup();
    ids
}

/// Returns HTTP 409 `consent_required` JSON when enabling a plugin needs a grant.
fn consent_required_response(plugin_id: &str, message: String, summary: Vec<String>) -> Response {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({
            "error": "consent_required",
            "message": message,
            "plugin_id": plugin_id,
            "summary": summary,
        })),
    )
        .into_response()
}

/// Restrictive CSP for SVG logos served from the daemon origin (defense in depth
/// alongside [`sanitize_svg_logo`]).
///
/// `img-src data: blob:` allows sanitized embedded rasters (`allow_standard_images`);
/// `style-src 'unsafe-inline'` covers typical SVG presentation attributes/styles.
/// Scripts and plugins stay blocked.
const SVG_LOGO_CSP: &str = "default-src 'none'; img-src data: blob:; style-src 'unsafe-inline'; script-src 'none'; object-src 'none'; sandbox";

/// Serve an embedded `plugin.toml` `logo` file from under the plugin install root.
#[utoipa::path(
    get,
    path = "/api/plugins/{kind}/{id}/logo",
    params(("kind" = String, Path, description = "Plugin kind (`source`, `destination`, `integration`, …)."), ("id" = String, Path, description = "Plugin id.")),
    responses(
        (status = 200, description = "The logo (PNG, or SVG served with a restrictive CSP).", content(
            (crate::openapi::BinaryBody = "image/png"),
            (crate::openapi::BinaryBody = "image/svg+xml"),
        )),
        (status = 404, description = "The plugin has no logo.", body = crate::http_error::ErrorJson),
    ),
)]
async fn get_plugin_logo(
    State(state): State<Arc<AppState>>,
    AxumPath((kind, id)): AxumPath<(String, String)>,
) -> Result<Response, StatusCode> {
    let cfg = state.config.read().await.clone();
    let (bytes, content_type) = plugin_logo_bytes_for(&cfg, &kind, &id)?;
    Ok(plugin_logo_response(bytes, content_type))
}

/// Build the HTTP response for an embedded logo (SVG gets CSP; never raw SVG).
fn plugin_logo_response(bytes: Vec<u8>, content_type: &'static str) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=3600"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if content_type == "image/svg+xml" {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(SVG_LOGO_CSP),
        );
    }
    (StatusCode::OK, headers, bytes).into_response()
}

/// Discover `family`/`id` and read an embedded logo (sync; route + integration tests).
fn plugin_logo_bytes_for(
    config: &Config,
    family: &str,
    id: &str,
) -> Result<(Vec<u8>, &'static str), StatusCode> {
    let plugin = bookclerk_plugin_host::discover_plugins(config)
        .map_err(|_| StatusCode::NOT_FOUND)?
        .into_iter()
        .find(|p| {
            p.manifest.id == id
                && p.manifest
                    .families()
                    .iter()
                    .any(|f| plugin_family_label(*f) == family)
        })
        .ok_or(StatusCode::NOT_FOUND)?;
    read_embedded_plugin_logo(&plugin)
}

/// Read and confine an embedded logo path under `plugin.root`.
///
/// SVG bytes are always run through Cloudflare `svg-hush` before return; filter
/// failure yields [`StatusCode::UNPROCESSABLE_ENTITY`] (never the raw input).
fn read_embedded_plugin_logo(
    plugin: &bookclerk_plugin_host::DiscoveredPlugin,
) -> Result<(Vec<u8>, &'static str), StatusCode> {
    let raw = plugin
        .manifest
        .logo
        .as_deref()
        .ok_or(StatusCode::NOT_FOUND)?;
    let logo = bookclerk_plugin_host::validate_logo(raw).map_err(|_| StatusCode::NOT_FOUND)?;
    let bookclerk_plugin_host::LogoKind::EmbeddedPath(rel) = logo else {
        return Err(StatusCode::NOT_FOUND);
    };
    let root = std::fs::canonicalize(&plugin.root).map_err(|_| StatusCode::NOT_FOUND)?;
    let path = std::fs::canonicalize(root.join(&rel)).map_err(|_| StatusCode::NOT_FOUND)?;
    if !path.starts_with(&root) {
        return Err(StatusCode::NOT_FOUND);
    }
    let meta = std::fs::metadata(&path).map_err(|_| StatusCode::NOT_FOUND)?;
    if !meta.is_file() {
        return Err(StatusCode::NOT_FOUND);
    }
    if meta.len() > bookclerk_plugin_host::MAX_EMBEDDED_LOGO_BYTES {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }
    let bytes = std::fs::read(&path).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let content_type = bookclerk_plugin_host::logo_content_type(&rel);
    if content_type == "image/svg+xml" || rel.to_ascii_lowercase().ends_with(".svg") {
        let sanitized = sanitize_svg_logo(&bytes)?;
        Ok((sanitized, "image/svg+xml"))
    } else {
        Ok((bytes, content_type))
    }
}

/// Strip scripting and other abusable SVG features. On error, callers must not
/// serve `input`.
fn sanitize_svg_logo(input: &[u8]) -> Result<Vec<u8>, StatusCode> {
    let mut filter = svg_hush::Filter::new();
    filter.set_data_url_filter(svg_hush::data_url_filter::allow_standard_images);
    let mut out = Vec::new();
    filter.filter(input, &mut out).map_err(|err| {
        tracing::warn!(error = %err, "svg-hush rejected embedded plugin logo");
        StatusCode::UNPROCESSABLE_ENTITY
    })?;
    Ok(out)
}

/// Resolves consent-dialog branding from the live guest, falling back to the manifest logo.
async fn plugin_consent_brand(
    state: &AppState,
    plugin: &bookclerk_plugin_host::DiscoveredPlugin,
) -> Option<PluginConsentBrand> {
    let mut brand = PluginConsentBrand {
        name: plugin
            .manifest
            .name
            .clone()
            .unwrap_or_else(|| plugin.manifest.id.clone()),
        bg: None,
        fg: None,
        accent: None,
        logo: settings_logo_from_manifest(plugin),
    };
    let families = plugin.manifest.families();
    if families.contains(&bookclerk_plugin_host::PluginFamily::Source) {
        let source = state.sources.read().await.get(&plugin.manifest.id);
        if let Some(source) = source {
            let source_brand = source.portal_brand();
            brand.name = source_brand.name.to_string();
            brand.bg = Some(source_brand.bg.to_string());
            brand.fg = Some(source_brand.fg.to_string());
            brand.accent = Some(source_brand.accent.to_string());
            brand.logo = non_empty_logo(source_brand.logo_href()).or(brand.logo);
        }
    } else if families.contains(&bookclerk_plugin_host::PluginFamily::Integration) {
        let integration = state.integrations.read().await.get(&plugin.manifest.id);
        if let Some(integration_brand) =
            integration.and_then(|integration| integration.portal_brand())
        {
            brand.name = integration_brand.name.to_string();
            brand.bg = Some(integration_brand.bg.to_string());
            brand.fg = Some(integration_brand.fg.to_string());
            brand.accent = Some(integration_brand.accent.to_string());
            brand.logo = non_empty_logo(integration_brand.logo_href()).or(brand.logo);
        }
    }
    if brand.name == plugin.manifest.id
        && brand.bg.is_none()
        && brand.fg.is_none()
        && brand.accent.is_none()
        && brand.logo.is_none()
    {
        None
    } else {
        brand.logo = brand
            .logo
            .take()
            .and_then(|logo| state.covers.same_origin_src(&logo));
        Some(brand)
    }
}

/// Builds host-capped consent sliders from workerd limits and jail defaults.
fn plugin_consent_limits(
    plugin: &bookclerk_plugin_host::DiscoveredPlugin,
    jail_cpu_rate_percent: Option<u32>,
) -> PluginConsentLimits {
    let (cpu_ms, subrequests) = if plugin.manifest.runtime == PluginRuntimeKind::Workerd {
        let effective = plugin
            .manifest
            .workerd
            .as_ref()
            .map(|workerd| workerd.limits.clone())
            .unwrap_or_default()
            .effective();
        (effective.cpu_ms, effective.subrequests)
    } else {
        (
            WorkerdLimits::DEFAULT_CPU_MS,
            WorkerdLimits::DEFAULT_SUBREQUESTS,
        )
    };
    let max_cpu_cores = host_cpu_cores_max();
    PluginConsentLimits {
        cpu_ms,
        subrequests,
        max_cpu_ms: WorkerdLimits::MAX_CPU_MS,
        max_subrequests: WorkerdLimits::MAX_SUBREQUESTS,
        disk_mib: PLUGIN_STATE_BUDGET_MIB_DEFAULT,
        max_disk_mib: PLUGIN_STATE_BUDGET_MIB_MAX,
        memory_mib: PLUGIN_JAIL_MEMORY_MIB_DEFAULT,
        max_memory_mib: PLUGIN_JAIL_MEMORY_MIB_MAX,
        cpu_cores: PLUGIN_JAIL_CPU_CORES_DEFAULT.min(max_cpu_cores),
        max_cpu_cores,
        jail_cpu_cores: jail_cpu_rate_percent
            .map(|n| effective_cpu_cores(Some(percent_to_cores(n)))),
        extra_processes: PLUGIN_JAIL_EXTRA_PROCESSES_DEFAULT,
        max_extra_processes: PLUGIN_JAIL_EXTRA_PROCESSES_MAX,
        known_bindings: KNOWN_HOST_BINDINGS.to_vec(),
    }
}

/// Merges operator overrides onto the manifest grant and rejects values outside host caps.
fn build_approved_grant(
    baseline: &PluginGrant,
    grant_override: Option<PluginGrantOverride>,
) -> Result<PluginGrant, StatusCode> {
    let Some(grant_override) = grant_override else {
        return Ok(baseline.clone());
    };
    let mut approved = baseline.clone();
    if let Some(network_mode) = grant_override.network_mode {
        approved.network_mode = network_mode;
    }
    if let Some(domains) = grant_override.domains {
        approved.domains = vec_to_set(domains);
    }
    if let Some(tcp) = grant_override.tcp {
        approved.tcp = tcp
            .into_iter()
            .map(|t| bookclerk_plugin_host::TcpGrant {
                host: t.host,
                ports: t.ports,
            })
            .collect();
    }
    if let Some(cidrs) = grant_override.address_cidrs {
        approved.address_cidrs = vec_to_set(cidrs);
    }
    if let Some(flag) = grant_override.allow_undeclared_public_redirects {
        approved.allow_undeclared_public_redirects = flag;
        // Record an operator choice only when it departs from the manifest.
        // Echoing the baseline back (the dialog always submits the box) is
        // not an answer, so a later version that asks for redirects still
        // waits for consent instead of reading as already refused.
        approved.operator_allow_undeclared_public_redirects =
            (flag != baseline.allow_undeclared_public_redirects).then_some(flag);
    }
    if let Some(hosts) = grant_override.own_user_agent_hosts {
        approved.own_user_agent_hosts = Some(vec_to_set(hosts));
    }
    if let Some(bindings) = grant_override.bindings {
        approved.bindings = vec_to_set(bindings);
    }
    if let Some(compatibility_flags) = grant_override.compatibility_flags {
        approved.compatibility_flags = vec_to_set(compatibility_flags);
    }
    if grant_override.cpu_ms.is_some() {
        approved.cpu_ms = grant_override.cpu_ms;
    }
    if grant_override.subrequests.is_some() {
        approved.subrequests = grant_override.subrequests;
    }
    if grant_override.disk_mib.is_some() {
        approved.disk_mib = grant_override.disk_mib;
    }
    if grant_override.memory_mib.is_some() {
        approved.memory_mib = grant_override.memory_mib;
    }
    if let Some(cores) = grant_override.cpu_cores {
        let percent = cores_to_percent(effective_cpu_cores(Some(cores)));
        approved.cpu_rate_percent = Some(percent.max(1));
    }
    if grant_override.extra_processes.is_some() {
        approved.extra_processes = grant_override.extra_processes;
    }
    validate_approved_grant(&approved, baseline).map_err(|err| {
        tracing::warn!(error = %err, plugin = %baseline.plugin_id, "invalid plugin consent grant override");
        StatusCode::BAD_REQUEST
    })
}

/// Trims, drops empties, and collects strings into a `BTreeSet`.
fn vec_to_set(values: Vec<String>) -> BTreeSet<String> {
    values
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

/// Optional query for provenance-qualified consent lookup.
#[derive(Debug, Deserialize, Default, utoipa::ToSchema, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct PluginConsentQuery {
    /// Canonical PluginKey; when set, wins over the path alias.
    #[serde(default)]
    plugin_key: Option<String>,
}

/// Resolves a Settings/consent plugin by PluginKey or unambiguous alias.
fn resolve_consent_plugin<'a>(
    plugins: &'a [bookclerk_plugin_host::DiscoveredPlugin],
    id: &str,
    plugin_key: Option<&str>,
) -> Result<&'a bookclerk_plugin_host::DiscoveredPlugin, StatusCode> {
    let spec = plugin_key
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(id);
    bookclerk_plugin_host::resolve_plugin_ref(plugins, spec).map_err(|err| {
        let message = err.to_string();
        if message.contains("ambiguous") {
            StatusCode::CONFLICT
        } else {
            StatusCode::NOT_FOUND
        }
    })
}

/// Loads the consent dialog for one plugin, including coverage against the stored grant.
#[utoipa::path(
    get,
    path = "/api/plugins/{id}/consent",
    params(("id" = String, Path, description = "Plugin id."), PluginConsentQuery),
    responses(
        (status = 200, description = "What the plugin asks for and what is granted.", body = PluginConsentResponse),
        (status = 404, description = "No such plugin.", body = crate::http_error::ErrorJson),
        (status = 500, description = "The grant could not be read.", body = crate::http_error::ErrorJson),
    ),
)]
async fn get_plugin_consent(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<PluginConsentQuery>,
) -> Result<Json<PluginConsentResponse>, StatusCode> {
    let cfg = state.config.read().await.clone();
    let discovered = discover_plugins_for_settings(&cfg).await;
    let plugin = resolve_consent_plugin(&discovered, &id, query.plugin_key.as_deref())?;
    let request = consent_request(&plugin.manifest, plugin.plugin_key());
    let summary = consent_summary(&request);
    let store = PluginGrantStore::load(&cfg.paths().files_dir).map_err(|err| {
        tracing::error!(error = %err, "failed to load plugin grants");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let existing = store
        .get_by_plugin_key(plugin.plugin_key().canonical())
        .cloned();
    let pending = existing
        .as_ref()
        .map(|grant| pending_structural(grant, &request));
    let covered = existing.as_ref().is_some_and(|grant| {
        grant_covers(grant, &request) && pending.as_ref().is_some_and(|p| p.is_empty())
    });
    let brand = plugin_consent_brand(&state, plugin).await;
    let limits = plugin_consent_limits(plugin, cfg.plugins.jail.cpu_rate_percent);
    let runtime = match plugin.manifest.runtime {
        PluginRuntimeKind::Native => "native",
        PluginRuntimeKind::Workerd => "workerd",
    }
    .to_string();
    Ok(Json(PluginConsentResponse {
        plugin_id: plugin.alias().to_string(),
        plugin_key: plugin.plugin_key().canonical().to_string(),
        provenance: plugin.identity.provenance.to_string(),
        runtime,
        request: PluginGrantView::from_grant(&request),
        covered,
        summary,
        existing: existing.as_ref().map(PluginGrantView::from_grant),
        pending: pending.as_ref().map(PluginConsentPending::from),
        brand,
        limits,
        host_user_agent: bookclerk_plugin_manifest::host_user_agent(),
    }))
}

/// Persists an approved plugin grant after validating operator overrides.
#[utoipa::path(
    post,
    path = "/api/plugins/{id}/consent",
    params(("id" = String, Path, description = "Plugin id."), PluginConsentQuery),
    request_body = PluginConsentApproveRequest,
    responses(
        (status = 200, description = "The grant was stored.", body = PluginConsentResponse),
        (status = 400, description = "An override is not allowed.", body = crate::http_error::ErrorJson),
        (status = 404, description = "No such plugin.", body = crate::http_error::ErrorJson),
        (status = 500, description = "The grant could not be stored.", body = crate::http_error::ErrorJson),
    ),
)]
async fn post_plugin_consent(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<PluginConsentQuery>,
    Json(body): Json<PluginConsentApproveRequest>,
) -> Result<Json<PluginConsentResponse>, StatusCode> {
    if !body.approve {
        return Err(StatusCode::BAD_REQUEST);
    }
    let cfg = state.config.read().await.clone();
    let discovered = discover_plugins_for_settings(&cfg).await;
    let plugin = resolve_consent_plugin(&discovered, &id, query.plugin_key.as_deref())?;
    let request = consent_request(&plugin.manifest, plugin.plugin_key());
    let summary = consent_summary(&request);
    let approved = build_approved_grant(&request, body.grant)?;
    let mut store = PluginGrantStore::load(&cfg.paths().files_dir).map_err(|err| {
        tracing::error!(error = %err, "failed to load plugin grants");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    store.upsert(approved.clone());
    store.save(&cfg.paths().files_dir).map_err(|err| {
        tracing::error!(error = %err, "failed to save plugin grants");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let brand = plugin_consent_brand(&state, plugin).await;
    let limits = plugin_consent_limits(plugin, cfg.plugins.jail.cpu_rate_percent);
    let runtime = match plugin.manifest.runtime {
        PluginRuntimeKind::Native => "native",
        PluginRuntimeKind::Workerd => "workerd",
    }
    .to_string();
    // An approval can leave a structural capability unticked; it stays
    // pending, so the answer says so instead of claiming coverage.
    let pending = pending_structural(&approved, &request);
    Ok(Json(PluginConsentResponse {
        plugin_id: plugin.alias().to_string(),
        plugin_key: plugin.plugin_key().canonical().to_string(),
        provenance: plugin.identity.provenance.to_string(),
        runtime,
        request: PluginGrantView::from_grant(&request),
        covered: pending.is_empty(),
        summary,
        existing: Some(PluginGrantView::from_grant(&approved)),
        pending: Some(PluginConsentPending::from(&pending)),
        brand,
        limits,
        host_user_agent: bookclerk_plugin_manifest::host_user_agent(),
    }))
}

/// Returns file-backed and runtime-effective Settings plus per-plugin groups.
#[utoipa::path(
    get,
    path = "/api/settings",
    responses(
        (status = 200, description = "The settings.", body = SettingsResponse),
        (status = 500, description = "The settings could not be read.", body = crate::http_error::ErrorJson),
    ),
)]
async fn get_settings(
    State(state): State<Arc<AppState>>,
) -> Result<Json<SettingsResponse>, StatusCode> {
    let cfg = state.config.read().await.clone();
    let library = state.library_snapshot().await;
    let discovered_plugins = discover_plugins_for_settings(&cfg).await;
    // A config without resolved paths (unit tests) has no node identity and
    // therefore no per-host deployment view; the plugin groups still render.
    let node_id = cfg
        .paths
        .as_ref()
        .and_then(|paths| {
            bookclerk_library::control_plane::load_or_create_node_identity(&paths.files_dir)
                .map_err(|err| {
                    tracing::warn!(error = %err, "node identity unavailable for settings");
                })
                .ok()
        })
        .map(|host| host.node_id)
        .unwrap_or_default();
    let auth = state.auth_snapshot().await;
    let mut settings = current_settings_snapshot(&cfg);
    // `output.*` comes from the `core.output` document itself, so a relative
    // `widevine_cdm` reads back as stored rather than resolved for this host.
    let output_revision = match bookclerk_library::control_plane::load_output(&library).await {
        Ok(doc) => {
            insert_output_settings(&mut settings, &doc.body);
            Some(doc.revision)
        }
        Err(err) => {
            tracing::warn!(error = %err, "core.output unavailable for settings");
            None
        }
    };
    // `cache.*` comes from the `core.cache` document, or its defaults when
    // nobody has written one.
    match bookclerk_library::control_plane::load_cache_settings(&library).await {
        Ok(doc) => {
            let body = doc.map(|doc| doc.body).unwrap_or_default();
            for key in bookclerk_library::control_plane::CACHE_SETTING_KEYS {
                if let Some(value) = body.get(key) {
                    settings.insert((*key).to_string(), value);
                }
            }
        }
        Err(err) => tracing::warn!(error = %err, "core.cache unavailable for settings"),
    }
    let mut effective = settings.clone();
    effective.insert("daemon.auth.enabled".into(), auth.enabled.to_string());
    effective.insert("daemon.listen".into(), cfg.daemon.listen.join_comma());
    effective.insert("node.id".into(), node_id.clone());
    for source in state.sources.read().await.all() {
        let address = source
            .plugin_instance_id()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| source.id());
        effective.insert(format!("runtime.sources.{address}.loaded"), "true".into());
    }
    for integration in state.integrations.read().await.all() {
        let address = integration
            .plugin_instance_id()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| integration.id());
        effective.insert(
            format!("runtime.integrations.{address}.loaded"),
            "true".into(),
        );
    }
    if let Some(revision) = cfg.events_revision {
        effective.insert("events.revision".into(), revision.to_string());
    }
    if let Some(revision) = cfg.output_revision {
        effective.insert("output.revision".into(), revision.to_string());
    }
    let mut plugins = plugin_settings_snapshot(&cfg, &library, &node_id, &discovered_plugins).await;
    for group in &mut plugins {
        group.logo = group
            .logo
            .take()
            .and_then(|logo| state.covers.same_origin_src(&logo));
    }
    Ok(Json(SettingsResponse {
        settings,
        effective,
        plugins,
        host_cpu_cores_max: host_cpu_cores_max(),
        jail_cpu_cores: cfg
            .plugins
            .jail
            .cpu_rate_percent
            .map(|n| effective_cpu_cores(Some(percent_to_cores(n)))),
        output_revision,
    }))
}

/// Validates, writes, and reloads allowlisted Settings keys under `reload_lock`.
#[utoipa::path(
    patch,
    path = "/api/settings",
    request_body = PatchSettingsRequest,
    responses(
        (status = 200, description = "The settings after the change.", body = SettingsResponse),
        (status = 400, description = "A key is unknown or a value is invalid.", body = crate::http_error::ErrorJson),
        (status = 409, description = "`output.*` keys were sent with a stale `expected_output_revision`, or another write won; the body's `current_revision` is the revision to retry against (`revision_conflict`)."),
        (status = 500, description = "The settings could not be written.", body = crate::http_error::ErrorJson),
    ),
)]
async fn patch_settings(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PatchSettingsRequest>,
) -> Result<Json<SettingsResponse>, Response> {
    if body.settings.is_empty() {
        return get_settings(State(state.clone()))
            .await
            .map_err(IntoResponse::into_response);
    }

    let body_expected_output_revision = body.expected_output_revision;
    let updates: Vec<(String, String)> = body
        .settings
        .into_iter()
        .map(|update| {
            let key = update.key.trim().to_string();
            let normalized = normalize_setting_value(&key, &update.value)?;
            if !allowed_setting_key(&key) {
                return Err(format!("unsupported setting key: {key}"));
            }
            Ok((key, normalized))
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| {
            tracing::warn!(error = %err, "rejected invalid settings update");
            StatusCode::BAD_REQUEST.into_response()
        })?;

    let editable = get_settings(State(state.clone()))
        .await
        .map_err(IntoResponse::into_response)?
        .0;
    // Plugin-instance groups are edited through the plugin-instance routes;
    // only `config.toml`-backed groups (database adapters) are patchable here.
    let editable_keys: std::collections::HashSet<&str> = editable
        .plugins
        .iter()
        .filter(|plugin| plugin.plugin_instance_id.is_none())
        .flat_map(|plugin| plugin.settings.iter().map(|setting| setting.key.as_str()))
        .chain(editable.settings.keys().map(String::as_str))
        .collect();
    let editable_plugin_options: std::collections::HashMap<&str, &PluginSettingOption> = editable
        .plugins
        .iter()
        .filter(|plugin| plugin.plugin_instance_id.is_none())
        .flat_map(|plugin| plugin.settings.iter())
        .map(|setting| (setting.key.as_str(), setting))
        .collect();
    if let Some((key, _)) = updates
        .iter()
        .find(|(key, _)| !editable_keys.contains(key.as_str()) && key != "database.plugin")
    {
        tracing::warn!(%key, "rejected unsupported settings update key");
        return Err(StatusCode::BAD_REQUEST.into_response());
    }
    if let Some((key, value)) = updates.iter().find(|(key, value)| {
        editable_plugin_options
            .get(key.as_str())
            .and_then(|setting| setting.choices.as_ref())
            .is_some_and(|choices| !choices.iter().any(|choice| choice.value == *value))
    }) {
        tracing::warn!(%key, %value, "rejected invalid enum settings update value");
        return Err(StatusCode::BAD_REQUEST.into_response());
    }

    let files_dir = {
        let cfg = state.config.read().await;
        cfg.paths().files_dir.clone()
    };
    let config_path = {
        let cfg = state.config.read().await;
        cfg.paths().config_file.clone()
    };
    let current_database_plugin = {
        let cfg = state.config.read().await;
        cfg.database.plugin.clone()
    };

    let mut enabling = newly_enabled_plugin_ids(&updates, &editable_plugin_options);
    // Direct `database.plugin` (and exclusive `database.<id>.enabled`) switches
    // must not bypass consent — same bar as `bookclerk plugins deploy`.
    for id in database_backends_requiring_grant(&updates, &current_database_plugin) {
        if !enabling.iter().any(|e| e.eq_ignore_ascii_case(&id)) {
            enabling.push(id);
        }
    }
    let discovered = discover_plugins_for_settings(
        &Config::load(Some(files_dir.clone()), Some(config_path.clone())).map_err(|err| {
            tracing::error!(error = %err, "failed to load config for consent check");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        })?,
    )
    .await;
    if !enabling.is_empty() {
        for plugin_id in &enabling {
            let plugin = match bookclerk_plugin_host::resolve_plugin_ref(&discovered, plugin_id) {
                Ok(plugin) => plugin,
                Err(err) => {
                    let message = err.to_string();
                    tracing::warn!(%plugin_id, error = %err, "cannot enable plugin");
                    let status = if message.contains("ambiguous") {
                        StatusCode::CONFLICT
                    } else {
                        StatusCode::BAD_REQUEST
                    };
                    return Err(status.into_response());
                }
            };
            if let Err(err) = require_grant(&files_dir, plugin) {
                let request = consent_request(&plugin.manifest, plugin.plugin_key());
                let summary = consent_summary(&request);
                tracing::warn!(%plugin_id, error = %err, "plugin enable blocked pending consent");
                return Err(consent_required_response(
                    plugin_id,
                    err.to_string(),
                    summary,
                ));
            }
        }
    }

    let mut normalized_pairs = Vec::<(String, String)>::new();
    let mut events_updates = Vec::<(String, String)>::new();
    let mut output_updates = Vec::<(String, String)>::new();
    let mut cache_updates = Vec::<(String, String)>::new();
    for (key, value) in &updates {
        if key.starts_with("database.") && key.ends_with(".enabled") {
            // Exclusive enablement is applied via `apply_database_enable_updates`.
            continue;
        }
        match settings_authority(key) {
            SettingsAuthority::Events => events_updates.push((key.clone(), value.clone())),
            SettingsAuthority::Output => output_updates.push((key.clone(), value.clone())),
            SettingsAuthority::Cache => cache_updates.push((key.clone(), value.clone())),
            SettingsAuthority::File => normalized_pairs.push((key.clone(), value.clone())),
        }
    }

    // `core.events`, `core.output`, `core.cache`, and file-backed keys are different
    // authorities, each committed separately. A later failure on one must not
    // leave a commit on another behind, so a mixed request is rejected before
    // any side is written.
    let authorities: std::collections::HashSet<SettingsAuthority> = updates
        .iter()
        .map(|(key, _)| settings_authority(key))
        .collect();
    if authorities.len() > 1 {
        tracing::warn!("rejected settings update that mixes configuration authorities");
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "mixed_configuration_authorities",
                "message": "send core.events, core.output, core.cache, and file-backed settings in separate requests",
            })),
        )
            .into_response());
    }
    let _reload_guard = state.reload_lock.lock().await;
    if !events_updates.is_empty() {
        crate::config_authority::commit_events_settings(&state, &events_updates).await?;
    }
    if !output_updates.is_empty() {
        crate::config_authority::commit_output_settings(
            &state,
            &output_updates,
            body_expected_output_revision,
        )
        .await?;
    }
    if !cache_updates.is_empty() {
        crate::config_authority::commit_cache_settings(&state, &cache_updates).await?;
    }
    if normalized_pairs.is_empty() && enabling.is_empty() {
        drop(_reload_guard);
        return get_settings(State(state))
            .await
            .map_err(IntoResponse::into_response);
    }

    let mut cfg = Config::load(Some(files_dir), Some(config_path.clone())).map_err(|err| {
        tracing::error!(error = %err, "failed to load config for settings update");
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    })?;

    apply_database_enable_updates(&mut cfg, &updates, &discovered).map_err(|err| {
        tracing::warn!(error = %err, "rejected database settings update");
        StatusCode::BAD_REQUEST.into_response()
    })?;

    let pairs = normalized_pairs
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect::<Vec<_>>();
    bookclerk_config::apply_setting_overrides(&mut cfg, &pairs);

    // Validate listen (and auth/listen pairing) before touching disk so a bad
    // address cannot leave config.toml half-updated.
    if let Err(err) = validate_daemon_listen(&cfg) {
        tracing::warn!(error = %err, "rejected daemon.listen settings update");
        return Err(StatusCode::BAD_REQUEST.into_response());
    }

    if let Err(err) = stamp_settings_occupancy(&mut cfg, &discovered, &enabling) {
        tracing::warn!(error = %err, "cannot stamp occupancy for enable");
        return Err(StatusCode::BAD_REQUEST.into_response());
    }

    cfg.write_toml_file(&config_path).map_err(|err| {
        tracing::error!(error = %err, "settings update write failed");
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    })?;

    reload_daemon_config_held(&state).await.map_err(|err| {
        tracing::error!(error = %err, "settings reload failed");
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    })?;
    drop(_reload_guard);

    get_settings(State(state))
        .await
        .map_err(IntoResponse::into_response)
}

/// Reloads `config.toml` and republishes daemon runtime without a process restart.
#[utoipa::path(
    post,
    path = "/api/config/reload",
    responses(
        (status = 200, description = "The configuration was reloaded.", body = ActionResponse, example = json!({"ok": true, "message": "reloaded /home/bookclerk/BookclerkFiles/config.toml (master.key wrap=true)", "job_id": ""})),
        (status = 500, description = "The reload failed; the old configuration stays live.", body = crate::http_error::ErrorJson),
    ),
)]
async fn reload_config(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ActionResponse>, StatusCode> {
    match reload_daemon_config(&state).await {
        Ok(message) => Ok(Json(ActionResponse {
            ok: true,
            message,
            job_id: String::new(),
            error: None,
        })),
        Err(err) => {
            tracing::error!(error = %err, "config reload failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Write `[database].plugin` from a **fresh** config snapshot taken while
/// holding `reload_lock`.
///
/// The migrate copy can run for a long time; callers must not apply a config
/// cloned before that await. After the lock, load the then-current file so a
/// concurrent OIDC PUT's secret generation is not rolled back.
pub(crate) async fn apply_migrated_database_plugin(
    state: &AppState,
    to_plugin: String,
) -> Result<PathBuf, StatusCode> {
    let _reload_guard = state.reload_lock.lock().await;
    let (files_dir, config_path) = {
        let cfg = state.config.read().await;
        (
            cfg.paths().files_dir.clone(),
            cfg.paths().config_file.clone(),
        )
    };
    let mut new_cfg = Config::load(Some(files_dir), Some(config_path.clone()))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    new_cfg.database.plugin = to_plugin;
    new_cfg
        .write_toml_file(&config_path)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    reload_daemon_config_held(state)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(config_path)
}

/// Copies library rows between database plugins and optionally switches `[database].plugin`.
#[utoipa::path(
    post,
    path = "/api/database/migrate",
    request_body = DatabaseMigrateRequest,
    responses(
        (status = 200, description = "The rows were copied.", body = ActionResponse, example = json!({"ok": true, "message": "copied 1284 row(s) from `sqlite` to `postgres`; updated [database].plugin, wrote /home/bookclerk/BookclerkFiles/config.toml, and reloaded library connection", "job_id": ""})),
        (status = 400, description = "The target is invalid.", body = crate::http_error::ErrorJson),
        (status = 500, description = "The copy failed.", body = crate::http_error::ErrorJson),
        (status = 409, description = "The migration target matches more than one database plugin."),
    ),
)]
async fn migrate_database(
    State(state): State<Arc<AppState>>,
    Json(body): Json<DatabaseMigrateRequest>,
) -> Result<Json<ActionResponse>, StatusCode> {
    let cfg = state.config.read().await.clone();
    let from_plugin = body
        .from
        .unwrap_or_else(|| cfg.database.plugin.clone())
        .trim()
        .to_string();
    let to_plugin = body.to.trim().to_string();
    match bookclerk_plugin_host::migrate_database_plugin(
        &cfg,
        &from_plugin,
        &to_plugin,
        &bookclerk_library::BackendMigrateOptions {
            dry_run: body.dry_run,
            force: body.force,
        },
    )
    .await
    {
        Ok(summary) => {
            let mut message = if body.dry_run {
                format!(
                    "dry-run: would copy {} row(s) from `{from_plugin}` to `{to_plugin}`",
                    summary.total_rows()
                )
            } else {
                format!(
                    "copied {} row(s) from `{from_plugin}` to `{to_plugin}`",
                    summary.total_rows()
                )
            };
            if body.apply && !body.dry_run {
                let files_dir = cfg.paths().files_dir.clone();
                let discovered = discover_plugins_for_settings(&cfg).await;
                let plugin =
                    match bookclerk_plugin_host::resolve_plugin_ref(&discovered, &to_plugin) {
                        Ok(plugin) => plugin,
                        Err(err) => {
                            tracing::warn!(
                                %to_plugin,
                                error = %err,
                                "cannot apply migrate to database plugin"
                            );
                            return Err(if err.to_string().contains("ambiguous") {
                                StatusCode::CONFLICT
                            } else {
                                StatusCode::BAD_REQUEST
                            });
                        }
                    };
                if let Err(err) = require_grant(&files_dir, plugin) {
                    tracing::warn!(
                        plugin = %to_plugin,
                        error = %err,
                        "database migrate apply blocked pending consent"
                    );
                    return Err(StatusCode::FORBIDDEN);
                }
                let path = apply_migrated_database_plugin(
                    &state,
                    plugin.plugin_key().canonical().to_string(),
                )
                .await?;
                message.push_str(&format!(
                    "; updated [database].plugin, wrote {}, and reloaded library connection",
                    path.display()
                ));
            }
            Ok(Json(ActionResponse {
                ok: true,
                message,
                job_id: String::new(),
                error: None,
            }))
        }
        Err(err) => {
            tracing::error!(error = %err, "database migrate failed");
            Err(StatusCode::BAD_REQUEST)
        }
    }
}

/// Returns library counts, listen addresses, storage backends, and whether operator auth is on.
#[utoipa::path(
    get,
    path = "/api/status",
    responses(
        (status = 200, description = "Library counts and daemon state.", body = StatusResponse),
        (status = 500, description = "The library could not be read.", body = crate::http_error::ErrorJson),
    ),
)]
async fn status(State(state): State<Arc<AppState>>) -> Result<Json<StatusResponse>, StatusCode> {
    let library = state.library_snapshot().await;
    let accounts = library
        .count_accounts()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)? as usize;
    let counts = library
        .count_books_by_status()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let books = usize::try_from(counts.total).unwrap_or(0);
    let acquired = counts.acquired;
    let pending = counts.not_acquired;
    let error = counts.error;
    let in_progress = counts.queued + counts.downloading;
    let (listen, storage_backend) = {
        let cfg = state.config.read().await;
        let destinations = state.destinations.read().await;
        let mut names = Vec::new();
        if destinations.local().is_some() {
            names.push("local");
        }
        if destinations.s3().is_some() {
            names.push("s3");
        }
        (
            cfg.daemon.listen.join_comma(),
            if names.is_empty() {
                "none".into()
            } else {
                names.join(",")
            },
        )
    };
    let auth_enabled = state.auth_snapshot().await.enabled;
    let event_metrics = library
        .event_delivery_metrics()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(StatusResponse {
        accounts,
        books,
        acquired,
        pending,
        error,
        in_progress,
        listen,
        storage_backend,
        auth_enabled,
        events: EventQueueStatus {
            pending: event_metrics.pending,
            running: event_metrics.running,
            suspended: event_metrics.suspended,
            dead_letter: event_metrics.dead_letter,
            acked: event_metrics.acked,
            oldest_pending_age_secs: event_metrics.oldest_pending_age_secs,
            retries_total: event_metrics.retries_total,
            suspensions_total: event_metrics.suspensions_total,
            dead_letters_total: event_metrics.dead_letters_total,
            dispatch_latency_ms_avg: event_metrics.dispatch_latency_ms_avg,
            handler_latency_ms_avg: event_metrics.handler_latency_ms_avg,
        },
    }))
}

/// Plans and enqueues a library scan; returns the accepted job ids.
///
/// The scan planner splits a scan of every account into one job per
/// scan-enabled account for operators, Owners and Administrators, and
/// limits anyone else to the accounts they own. `job_id` is the first
/// accepted (or already queued) job and `job_ids` lists every one.
#[utoipa::path(
    post,
    path = "/api/library/scan",
    request_body = Option<ScanRequest>,
    responses(
        (status = 200, description = "The scan jobs were accepted.", body = ScanPlanResponse, example = json!({"ok": true, "message": "scan planned: 2 accepted, 0 already pending or running, 0 not admitted (queue full)", "job_id": "scan-7d0c1e2f-3a4b-4c5d-8e6f-9a0b1c2d3e4f", "job_ids": ["scan-7d0c1e2f-3a4b-4c5d-8e6f-9a0b1c2d3e4f", "scan-8c1d2e3f-4a5b-4c6d-9e7f-0a1b2c3d4e5f"]})),
        (status = 400, description = "The body names no valid target.", body = ActionResponse, example = json!({"ok": false, "message": "the account named is blank", "job_id": "", "error": "invalid"})),
        (status = 403, description = "The caller may not scan these accounts.", body = ActionResponse, example = json!({"ok": false, "message": "this user may not scan the library", "job_id": "", "error": "forbidden"})),
        (status = 409, description = "A scan is already pending or running, or no node can run it.", body = PlanConflict, examples(("duplicate" = (summary = "Every planned job is already pending or running", value = json!({"ok": false, "message": "scan already pending or running", "job_id": "scan-7d0c1e2f-3a4b-4c5d-8e6f-9a0b1c2d3e4f", "job_ids": ["scan-7d0c1e2f-3a4b-4c5d-8e6f-9a0b1c2d3e4f"], "error": "conflict"}))), ("unroutable" = (summary = "No node runs every plugin instance the job requires", value = json!({"ok": false, "message": "no node runs all of the plugin instances this scan job requires (3f9c2b7e-1d4a-4c8e-9b2f-6a7d8e9f0a1b, 8c1d2e3f-4a5b-4c6d-9e7f-0a1b2c3d4e5f); deploy them to a common node", "job_id": "", "error": "unroutable"}))))),
        (status = 422, description = "No account matched.", body = ScanPlanResponse, example = json!({"ok": false, "message": "no scan-enabled store account to scan", "job_id": "", "job_ids": [], "error": "nothing_to_scan"})),
        (status = 429, description = "The job queue is full.", body = ScanPlanResponse, example = json!({"ok": false, "message": "job queue is full", "job_id": "", "job_ids": [], "error": "queue_full"})),
        (status = 503, description = "Job admission is paused.", body = ActionResponse, example = json!({"ok": false, "message": "job admission paused (database reload in progress)", "job_id": "", "error": "admission_paused"})),
        (status = 500, description = "The jobs could not be queued.", body = ActionResponse, example = json!({"ok": false, "message": "ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection", "job_id": "", "error": "internal_error"})),
    ),
)]
async fn trigger_scan(
    State(state): State<Arc<AppState>>,
    requester: Option<axum::Extension<ScanRequester>>,
    body: Option<Json<ScanRequest>>,
) -> Response {
    let Some(axum::Extension(requester)) = requester else {
        return (
            StatusCode::FORBIDDEN,
            Json(ActionResponse {
                ok: false,
                message: "scan requester unknown".into(),
                job_id: String::new(),
                error: Some("forbidden".into()),
            }),
        )
            .into_response();
    };
    let body = body.map(|Json(b)| b);
    let account = body.as_ref().and_then(|b| b.account.clone());
    let instance = body.and_then(|b| b.plugin_instance_id);
    match enqueue_scan(state, requester, account, instance, JobTrigger::Api).await {
        Ok(admits) => plan_response("scan", admits),
        Err(err) => match err.downcast_ref::<LibraryError>() {
            Some(LibraryError::Forbidden(message)) => (
                StatusCode::FORBIDDEN,
                Json(ActionResponse {
                    ok: false,
                    message: message.clone(),
                    job_id: String::new(),
                    error: Some("forbidden".into()),
                }),
            )
                .into_response(),
            Some(LibraryError::Invalid(message)) => (
                StatusCode::BAD_REQUEST,
                Json(ActionResponse {
                    ok: false,
                    message: message.clone(),
                    job_id: String::new(),
                    error: Some("invalid".into()),
                }),
            )
                .into_response(),
            _ => enqueue_error_response(err),
        },
    }
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// Result of a planned scan or acquire: [`ActionResponse`] plus every job id.
struct ScanPlanResponse {
    /// True when at least one job was accepted.
    #[schema(example = true)]
    ok: bool,
    /// Operator-facing status text.
    #[schema(
        example = "scan planned: 2 accepted, 0 already pending or running, 0 not admitted (queue full)"
    )]
    message: String,
    /// First accepted (or, when none was, already queued) job id.
    #[schema(example = "scan-7d0c1e2f-3a4b-4c5d-8e6f-9a0b1c2d3e4f")]
    job_id: String,
    /// Every accepted or already-queued job id, in plan order.
    #[schema(example = json!(["scan-7d0c1e2f-3a4b-4c5d-8e6f-9a0b1c2d3e4f", "scan-8c1d2e3f-4a5b-4c6d-9e7f-0a1b2c3d4e5f"]))]
    job_ids: Vec<String>,
    /// Machine slug when nothing was accepted (`conflict`, `queue_full`,
    /// `nothing_to_scan`, `nothing_to_acquire`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "nothing_to_scan")]
    error: Option<String>,
}

/// `POST /api/library/scan` and `POST /api/library/acquire` `409` body: the
/// two conflicts a plan can hit.
///
/// The schema is written by hand as an `anyOf`: a planned answer also
/// satisfies [`ActionResponse`] (every field of that schema is present), so
/// the derived `oneOf` would reject it.
#[derive(Debug, Serialize)]
#[serde(untagged)]
// Schema-only: `plan_response` and `enqueue_error_response` write the two
// payloads directly, so no variant is ever constructed.
#[allow(dead_code)]
enum PlanConflict {
    /// Every planned job is already pending or running
    /// ([`plan_response`]; `error` is `conflict`).
    Duplicate(ScanPlanResponse),
    /// No node runs every plugin instance a job requires
    /// ([`enqueue_error_response`]; `error` is `unroutable`).
    Unroutable(ActionResponse),
}

impl utoipa::PartialSchema for PlanConflict {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::schema::{AnyOfBuilder, RefBuilder};
        AnyOfBuilder::new()
            .item(
                RefBuilder::new()
                    .ref_location_from_schema_name("ScanPlanResponse")
                    .description(Some(
                        "Every planned job is already pending or running; `error` is `conflict`.",
                    )),
            )
            .item(
                RefBuilder::new()
                    .ref_location_from_schema_name("ActionResponse")
                    .description(Some(
                        "No node runs every plugin instance a job requires; `error` is \
                         `unroutable` and `message` names the instances.",
                    )),
            )
            .description(Some(
                "`POST /api/library/scan` and `POST /api/library/acquire` `409` body: the \
                 two conflicts a plan can hit.",
            ))
            .into()
    }
}

impl utoipa::ToSchema for PlanConflict {
    fn schemas(
        schemas: &mut Vec<(
            String,
            utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
        )>,
    ) {
        use utoipa::PartialSchema;
        schemas.push(("ScanPlanResponse".to_string(), ScanPlanResponse::schema()));
        schemas.push(("ActionResponse".to_string(), ActionResponse::schema()));
    }
}

/// Folds the admissions of one planned scan or acquire into one response.
///
/// The body is always a [`ScanPlanResponse`], so `job_ids` is present
/// whatever the job count. A single planned job keeps the status and
/// message of any other admission. Several answer 200 when any was
/// accepted, otherwise 409 when all were already queued and 429 when the
/// queue was full; an empty plan is 422 `nothing_to_scan` /
/// `nothing_to_acquire`.
fn plan_response(kind: &str, admits: Vec<AdmitJob>) -> Response {
    if let [admit] = admits.as_slice() {
        let (status, message, job_id, error) = match admit {
            AdmitJob::Created(id) => (
                StatusCode::OK,
                format!("{kind} job {id} accepted"),
                id.clone(),
                None,
            ),
            AdmitJob::Duplicate(id) => (
                StatusCode::CONFLICT,
                format!("{kind} already pending or running"),
                id.clone(),
                Some("conflict"),
            ),
            AdmitJob::QueueFull => (
                StatusCode::TOO_MANY_REQUESTS,
                "job queue is full".to_string(),
                String::new(),
                Some("queue_full"),
            ),
        };
        let job_ids = if job_id.is_empty() {
            Vec::new()
        } else {
            vec![job_id.clone()]
        };
        return (
            status,
            Json(ScanPlanResponse {
                ok: status == StatusCode::OK,
                message,
                job_id,
                job_ids,
                error: error.map(str::to_string),
            }),
        )
            .into_response();
    }
    // Ids stay in plan order, whatever each admission's outcome.
    let mut job_ids = Vec::new();
    let (mut created, mut duplicates, mut full) = (0usize, 0usize, 0usize);
    for admit in admits {
        match admit {
            AdmitJob::Created(id) => {
                created += 1;
                job_ids.push(id);
            }
            AdmitJob::Duplicate(id) => {
                duplicates += 1;
                job_ids.push(id);
            }
            AdmitJob::QueueFull => full += 1,
        }
    }
    let nothing = format!("nothing_to_{kind}");
    let (status, error) = if created > 0 {
        (StatusCode::OK, None)
    } else if job_ids.is_empty() && full == 0 {
        (StatusCode::UNPROCESSABLE_ENTITY, Some(nothing.as_str()))
    } else if full == 0 {
        (StatusCode::CONFLICT, Some("conflict"))
    } else {
        (StatusCode::TOO_MANY_REQUESTS, Some("queue_full"))
    };
    let message = if error == Some(nothing.as_str()) {
        if kind == "scan" {
            "no scan-enabled store account to scan".to_string()
        } else {
            "no store account of yours to acquire from".to_string()
        }
    } else {
        format!(
            "{kind} planned: {created} accepted, {duplicates} already pending or running, {full} not admitted (queue full)"
        )
    };
    (
        status,
        Json(ScanPlanResponse {
            ok: created > 0,
            message,
            job_id: job_ids.first().cloned().unwrap_or_default(),
            job_ids,
            error: error.map(str::to_string),
        }),
    )
        .into_response()
}

/// Plans and enqueues acquire jobs; returns the accepted job ids.
///
/// The acquire planner makes one job per title id, and limits a user who is
/// not an Owner or Administrator to the accounts they own (one job per
/// account when no title or account is named). `job_id` is the first
/// accepted (or already queued) job and `job_ids` lists every one.
#[utoipa::path(
    post,
    path = "/api/library/acquire",
    request_body = Option<AcquireRequestBody>,
    responses(
        (status = 200, description = "The acquire jobs were accepted.", body = ScanPlanResponse, example = json!({"ok": true, "message": "acquire job acquire-3f9c2b7e-1d4a-4c8e-9b2f-6a7d8e9f0a1b accepted", "job_id": "acquire-3f9c2b7e-1d4a-4c8e-9b2f-6a7d8e9f0a1b", "job_ids": ["acquire-3f9c2b7e-1d4a-4c8e-9b2f-6a7d8e9f0a1b"]})),
        (status = 400, description = "The body names no valid title.", body = ActionResponse, example = json!({"ok": false, "message": "every title id given is blank", "job_id": "", "error": "invalid"})),
        (status = 403, description = "The caller may not acquire for these accounts.", body = ActionResponse, example = json!({"ok": false, "message": "that title is not in a store account linked to this user", "job_id": "", "error": "forbidden"})),
        (status = 409, description = "The same acquire is already pending or running, or no node can run it.", body = PlanConflict, examples(("duplicate" = (summary = "Every planned job is already pending or running", value = json!({"ok": false, "message": "acquire already pending or running", "job_id": "acquire-7d0c1e2f-3a4b-4c5d-8e6f-9a0b1c2d3e4f", "job_ids": ["acquire-7d0c1e2f-3a4b-4c5d-8e6f-9a0b1c2d3e4f"], "error": "conflict"}))), ("unroutable" = (summary = "No node runs every plugin instance the job requires", value = json!({"ok": false, "message": "no node runs all of the plugin instances this acquire job requires (3f9c2b7e-1d4a-4c8e-9b2f-6a7d8e9f0a1b, 8c1d2e3f-4a5b-4c6d-9e7f-0a1b2c3d4e5f); deploy them to a common node", "job_id": "", "error": "unroutable"}))))),
        (status = 422, description = "No account of the caller's can acquire.", body = ScanPlanResponse, example = json!({"ok": false, "message": "no store account of yours to acquire from", "job_id": "", "job_ids": [], "error": "nothing_to_acquire"})),
        (status = 429, description = "The job queue is full.", body = ScanPlanResponse, example = json!({"ok": false, "message": "job queue is full", "job_id": "", "job_ids": [], "error": "queue_full"})),
        (status = 503, description = "Job admission is paused.", body = ActionResponse, example = json!({"ok": false, "message": "job admission paused (database reload in progress)", "job_id": "", "error": "admission_paused"})),
        (status = 500, description = "The jobs could not be queued.", body = ActionResponse, example = json!({"ok": false, "message": "ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection", "job_id": "", "error": "internal_error"})),
    ),
)]
async fn trigger_acquire(
    State(state): State<Arc<AppState>>,
    requester: Option<axum::Extension<ScanRequester>>,
    body: Option<Json<AcquireRequestBody>>,
) -> Response {
    let Some(axum::Extension(requester)) = requester else {
        return planner_error_response(None, "acquire requester unknown");
    };
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let mut titles: Vec<String> = body
        .uuid
        .or(body.asin)
        .or(body.isbn)
        .or(body.product_id)
        .into_iter()
        .collect();
    titles.extend(body.titles);
    let ask = crate::jobs::AcquireAsk {
        titles,
        account: body.account,
        plugin_instance_id: body.plugin_instance_id,
        force: body.force,
        pdf_only: body.pdf_only,
    };
    match enqueue_acquire(state, requester, ask, JobTrigger::Api).await {
        Ok(admits) => plan_response("acquire", admits),
        Err(err) => match err.downcast_ref::<LibraryError>() {
            Some(LibraryError::Forbidden(message)) => planner_error_response(None, message),
            Some(LibraryError::Invalid(message)) => {
                planner_error_response(Some(StatusCode::BAD_REQUEST), message)
            }
            _ => enqueue_error_response(err),
        },
    }
}

/// A planner refusal: 403 `forbidden` by default, or `status` with `invalid`.
fn planner_error_response(status: Option<StatusCode>, message: &str) -> Response {
    let (status, slug) = match status {
        Some(status) => (status, "invalid"),
        None => (StatusCode::FORBIDDEN, "forbidden"),
    };
    (
        status,
        Json(ActionResponse {
            ok: false,
            message: message.to_string(),
            job_id: String::new(),
            error: Some(slug.into()),
        }),
    )
        .into_response()
}

/// Maps a durable job row onto the control-plane `JobInfo` JSON.
fn job_info_from_record(job: JobRecord) -> JobInfo {
    let detail = job.progress.clone().or(job.error_message.clone());
    JobInfo {
        id: job.id,
        kind: job.kind,
        status: job.state,
        detail,
        progress: job.progress,
        attempt_count: job.attempt_count,
        max_attempts: job.max_attempts,
        error_kind: job.error_kind,
        created_at: Some(job.created_at.to_rfc3339()),
        finished_at: job.finished_at.map(|t| t.to_rfc3339()),
    }
}

/// Maps an admission outcome onto HTTP 200, 409, or 429 with `ActionResponse`.
fn admit_response(kind: &str, admit: AdmitJob) -> Response {
    match admit {
        AdmitJob::Created(id) => (
            StatusCode::OK,
            Json(ActionResponse {
                ok: true,
                message: format!("{kind} job {id} accepted"),
                job_id: id,
                error: None,
            }),
        )
            .into_response(),
        AdmitJob::Duplicate(id) => (
            StatusCode::CONFLICT,
            Json(ActionResponse {
                ok: false,
                message: format!("{kind} already pending or running"),
                job_id: id,
                error: Some("conflict".into()),
            }),
        )
            .into_response(),
        AdmitJob::QueueFull => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ActionResponse {
                ok: false,
                message: "job queue is full".into(),
                job_id: String::new(),
                error: Some("queue_full".into()),
            }),
        )
            .into_response(),
    }
}

/// Maps an admission error onto an HTTP response with `ActionResponse`.
///
/// A job no node could claim ([`crate::jobs::JobUnroutable`]) is HTTP 409
/// `unroutable` and logged as a warning; paused admission is HTTP 503
/// `admission_paused`; anything else is HTTP 500 `internal_error`.
fn enqueue_error_response(err: anyhow::Error) -> Response {
    let (status, error) = if err.downcast_ref::<crate::jobs::JobUnroutable>().is_some() {
        tracing::warn!(error = %err, "job refused: no node can run it");
        (StatusCode::CONFLICT, "unroutable")
    } else if err.to_string().contains("admission paused") {
        tracing::warn!(error = %err, "job enqueue paused");
        (StatusCode::SERVICE_UNAVAILABLE, "admission_paused")
    } else {
        tracing::error!(error = %err, "job enqueue failed");
        (StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
    };
    (
        status,
        Json(ActionResponse {
            ok: false,
            message: err.to_string(),
            job_id: String::new(),
            error: Some(error.into()),
        }),
    )
        .into_response()
}

/// Returns durable scan/acquire/listen_sync jobs, newest first.
#[utoipa::path(
    get,
    path = "/api/jobs",
    responses(
        (status = 200, description = "Jobs, newest first.", body = Vec<JobInfo>),
        (status = 500, description = "The job table could not be read.", body = crate::http_error::ErrorJson),
    ),
)]
async fn list_jobs(State(state): State<Arc<AppState>>) -> Result<Json<Vec<JobInfo>>, StatusCode> {
    let library = state.library_snapshot().await;
    let jobs = library
        .list_jobs(100)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(jobs.into_iter().map(job_info_from_record).collect()))
}

/// Operator JSON for one `domain_events` row.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct DomainEventInfo {
    /// Event id.
    #[schema(example = "b3f1c2d4-5e6f-4a7b-8c9d-0e1f2a3b4c5d")]
    id: String,
    /// Event type (`book_acquired`).
    #[schema(example = "book_acquired")]
    event_type: String,
    /// Payload schema version.
    #[schema(example = 1)]
    schema_version: i64,
    /// `pending` or `dispatched`.
    #[schema(example = "dispatched")]
    dispatch_state: String,
    /// Tenant / account id.
    #[schema(example = "reader@example.com")]
    account_id: String,
    /// Producer idempotency key.
    #[schema(example = "book_acquired:3f2c0d7e-9d4b-4a62-8b1e-6d0f7c2a9b31")]
    dedup_key: String,
    /// RFC 3339 insert time.
    #[schema(example = "2026-10-29T10:12:00Z")]
    created_at: String,
    /// Queue the row belongs to (`core.events` for domain events).
    #[schema(example = "core.events")]
    queue_name: String,
    /// Body encoding (`json`, `text`, `bytes`).
    #[schema(example = "json")]
    content_type: String,
    /// Producer plugin id; empty when the host published it.
    #[schema(example = "")]
    source: String,
    /// RFC 3339 earliest delivery time for delayed queue messages.
    #[schema(example = "2026-10-29T10:15:00Z")]
    deliver_after: Option<String>,
}

/// Operator JSON for one queue on `GET /api/queues`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct QueueInfo {
    /// Queue name.
    #[schema(example = "core.events")]
    name: String,
    /// True for the domain-event bus (`core.events`).
    #[schema(example = true)]
    core: bool,
    /// Deliveries waiting to be claimed (including suspended rows).
    #[schema(example = 3)]
    pending: i64,
    /// Deliveries currently claimed by a worker.
    #[schema(example = 1)]
    running: i64,
    /// Deliveries acked.
    #[schema(example = 1284)]
    acked: i64,
    /// Deliveries parked for operator review.
    #[schema(example = 0)]
    dead_letter: i64,
    /// Live catalog consumers (plugin keys or instance ids) of this queue.
    #[schema(example = json!(["platform:bookclerk/bookclerk-plugin-integration-audiobookshelf#audiobookshelf"]))]
    consumers: Vec<String>,
}

/// Operator JSON for `GET /api/queues/{name}`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct QueueDetail {
    /// Queue summary.
    #[serde(flatten)]
    queue: QueueInfo,
    /// Recent messages on the queue, newest first.
    messages: Vec<DomainEventInfo>,
    /// Recent deliveries on the queue, newest first.
    deliveries: Vec<EventDeliveryInfo>,
}

/// Operator JSON for one `event_deliveries` row.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct EventDeliveryInfo {
    /// Delivery id (`{event_id}:{plugin_id}`).
    #[schema(example = "b3f1c2d4-5e6f-4a7b-8c9d-0e1f2a3b4c5d:audiobookshelf")]
    id: String,
    /// Parent event id.
    #[schema(example = "b3f1c2d4-5e6f-4a7b-8c9d-0e1f2a3b4c5d")]
    event_id: String,
    /// Subscriber plugin id.
    #[schema(example = "audiobookshelf")]
    plugin_id: String,
    /// Lifecycle state.
    #[schema(example = "pending")]
    state: String,
    /// Claim count (not incremented on resume).
    #[schema(example = 1)]
    attempt_count: i64,
    /// Fence generation.
    #[schema(example = 2)]
    lease_generation: i64,
    /// Resume ordinal.
    #[schema(example = 0)]
    invocation_sequence: i64,
    /// Terminal outcome when set.
    #[schema(example = "ack")]
    outcome: Option<String>,
    /// Operator-facing error.
    #[schema(example = "integration API error (503): library scan in progress")]
    error_message: Option<String>,
    /// RFC 3339 earliest claim time.
    #[schema(example = "2026-10-29T10:12:00Z")]
    run_after: String,
    /// RFC 3339 last update.
    #[schema(example = "2026-10-29T10:12:30Z")]
    updated_at: String,
    /// Cooperative cancel flag.
    #[schema(example = false)]
    cancel_requested: bool,
    /// Concurrency class (`network` today).
    #[schema(example = "network")]
    resource_class: String,
    /// Next claim should resume rather than increment attempt.
    #[schema(example = false)]
    resume_pending: bool,
    /// True when a suspend checkpoint is stored.
    #[schema(example = false)]
    has_checkpoint: bool,
    /// Queue of the parent event.
    #[schema(example = "core.events")]
    queue_name: String,
}

/// Optional `state=` filter for `GET /api/events/deliveries`.
#[derive(Debug, Deserialize, utoipa::ToSchema, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct EventDeliveryQuery {
    /// Delivery state (`dead_letter`, `pending`, …).
    state: Option<String>,
}

/// Operator projection of one outbox row.
fn domain_event_info(e: bookclerk_library::DomainEventRecord) -> DomainEventInfo {
    DomainEventInfo {
        id: e.id,
        event_type: e.event_type,
        schema_version: e.schema_version,
        dispatch_state: e.dispatch_state,
        account_id: e.account_id,
        dedup_key: e.dedup_key,
        created_at: e.created_at.to_rfc3339(),
        queue_name: e.queue_name,
        content_type: e.content_type,
        source: e.source,
        deliver_after: e.deliver_after.map(|t| t.to_rfc3339()),
    }
}

/// Returns recent domain-event outbox rows, newest first.
#[utoipa::path(
    get,
    path = "/api/events",
    responses(
        (status = 200, description = "Recent domain events, newest first.", body = Vec<DomainEventInfo>),
        (status = 500, description = "The outbox could not be read.", body = crate::http_error::ErrorJson),
    ),
)]
async fn list_events(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<DomainEventInfo>>, StatusCode> {
    let library = state.library_snapshot().await;
    let rows = library
        .list_domain_events(100)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(rows.into_iter().map(domain_event_info).collect()))
}

/// Stored Discover state of the caller's shelves.
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct DiscoverStatusResponse {
    /// The caller's subject key (`operator`, `user:<id>`, `portal:<id>`).
    #[schema(example = "user:2")]
    subject: String,
    /// True once the caller has stored shelves to show.
    #[schema(example = true)]
    ready: bool,
    /// Seeds the stored feed was ranked from (`own` or `household`), when ranked.
    #[schema(example = "own")]
    scope: Option<String>,
    /// RFC 3339 time the stored feed was ranked, when ranked.
    #[schema(example = "2026-11-06T00:30:00+00:00")]
    feed_computed_at: Option<String>,
    /// RFC 3339 time the caller's last storefront fetch finished, when any.
    #[schema(example = "2026-11-06T00:29:41+00:00")]
    candidates_fetched_at: Option<String>,
    /// True while a fetch or ranking for the caller is waiting or running.
    #[schema(example = false)]
    updating: bool,
}

/// Reads the caller's Discover subject and reports its stored state.
///
/// When `queue` is set, the caller's fetch or ranking is queued when the
/// stored data is missing or stale, as a page read does; the refresh route
/// queues a fetch itself instead.
async fn discover_status_for_caller(
    state: &AppState,
    headers: &HeaderMap,
    queue: bool,
) -> Result<(bookclerk_library::DiscoverSubject, DiscoverStatusResponse), (StatusCode, String)> {
    let library = state.library_snapshot().await;
    let (subject_key, _) = auth::prefs_subject_for_caller(state, headers)
        .await
        .map_err(|status| (status, status.canonical_reason().unwrap_or("error").into()))?;
    let subject = bookclerk_library::DiscoverSubject::parse(&subject_key)
        .unwrap_or(bookclerk_library::DiscoverSubject::Operator);
    // A feed that does not decode (another release wrote it) counts as
    // missing, so it is ranked again and not reported ready. Status needs
    // only the feed's presence and times, so card metadata is not joined.
    let feed = bookclerk_discover::stored_subject_feed_state(&library, &subject)
        .await
        .map_err(internal_err)?;
    let fetched = library
        .newest_discover_candidate_fetch(&subject)
        .await
        .map_err(internal_err)?;
    if queue {
        let refresh =
            crate::discover_jobs::refresh_needed(feed.as_ref(), fetched, chrono::Utc::now());
        crate::discover_jobs::queue_refresh(state, &subject, refresh).await;
    }
    let updating = crate::discover_jobs::subject_refresh_active(&library, &subject)
        .await
        .map_err(internal_err)?;
    let status = DiscoverStatusResponse {
        subject: subject.key(),
        ready: feed.is_some(),
        scope: feed.as_ref().map(|f| f.scope.as_str().to_string()),
        feed_computed_at: feed.as_ref().map(|f| f.computed_at.to_rfc3339()),
        candidates_fetched_at: fetched.map(|t| t.to_rfc3339()),
        updating,
    };
    Ok((subject, status))
}

/// Queues a storefront fetch and ranking for the caller's shelves now, as
/// the Discover page's Refresh button asks. Unchanged store calls answer
/// from the day's cache, so a refresh is cheap; a fetch already waiting
/// absorbs the request.
#[utoipa::path(
    post,
    path = "/api/discover/refresh",
    responses(
        (status = 200, description = "Stored Discover state of the caller's shelves, with `updating` set once the fetch is queued.", body = DiscoverStatusResponse),
        (status = 401, description = "The caller has no portal identity.", body = String, content_type = "text/plain", example = json!("Unauthorized")),
        (status = 429, description = "The job queue is full, so the fetch was not queued.", body = String, content_type = "text/plain", example = json!("job queue is full")),
        (status = 500, description = "The lookup failed, or the fetch could not be queued.", body = String, content_type = "text/plain", example = json!("ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection")),
    ),
)]
async fn discover_refresh(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<DiscoverStatusResponse>, (StatusCode, String)> {
    let (subject, _) = discover_status_for_caller(&state, &headers, false).await?;
    // A fetch already waiting absorbs the request, so a duplicate counts as
    // queued; a full queue or a failed admission is reported.
    match crate::discover_jobs::enqueue_discover_candidates(
        &state,
        &subject,
        JobTrigger::Api,
        std::time::Duration::ZERO,
    )
    .await
    .map_err(internal_err)?
    {
        AdmitJob::Created(_) | AdmitJob::Duplicate(_) => {}
        AdmitJob::QueueFull => {
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                "job queue is full".to_string(),
            ))
        }
    }
    let (_, status) = discover_status_for_caller(&state, &headers, false).await?;
    Ok(Json(status))
}

/// Reports whether the caller's Discover shelves are stored yet, so
/// provisioning can wait for a new visitor's shelves before sending them in.
///
/// It also queues the caller's fetch or ranking when the stored data is
/// missing or stale, exactly as the Discover page read does.
#[utoipa::path(
    get,
    path = "/api/discover/status",
    responses(
        (status = 200, description = "Stored Discover state of the caller's shelves.", body = DiscoverStatusResponse),
        (status = 401, description = "The caller has no portal identity.", body = String, content_type = "text/plain", example = json!("Unauthorized")),
        (status = 500, description = "The lookup failed.", body = String, content_type = "text/plain", example = json!("ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection")),
    ),
)]
async fn discover_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<DiscoverStatusResponse>, (StatusCode, String)> {
    let (_, status) = discover_status_for_caller(&state, &headers, true).await?;
    Ok(Json(status))
}

/// Delivery counters plus live consumers for one queue.
async fn queue_info(
    library: &bookclerk_library::LibraryStore,
    catalog: &[bookclerk_library::EventSubscriberCatalogRecord],
    name: &str,
) -> Result<QueueInfo, StatusCode> {
    let count = |state: &'static str| async move {
        library
            .count_queue_deliveries(name, state)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
    };
    let mut consumers: Vec<String> = catalog
        .iter()
        .filter(|row| row.enabled && row.subscriptions.iter().any(|s| s.queue_name() == name))
        .map(|row| row.plugin_id.clone())
        .collect();
    consumers.sort();
    consumers.dedup();
    Ok(QueueInfo {
        name: name.to_string(),
        core: name == bookclerk_library::CORE_EVENTS_QUEUE,
        pending: count("pending").await?,
        running: count("running").await?,
        acked: count("acked").await?,
        dead_letter: count("dead_letter").await?,
        consumers,
    })
}

/// Returns every queue the outbox or the live catalog knows about, with
/// delivery counters and the consumers subscribed to it.
#[utoipa::path(
    get,
    path = "/api/queues",
    responses(
        (status = 200, description = "Every known queue.", body = Vec<QueueInfo>),
        (status = 500, description = "The outbox could not be read.", body = crate::http_error::ErrorJson),
    ),
)]
async fn list_queues(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<QueueInfo>>, StatusCode> {
    let library = state.library_snapshot().await;
    let catalog = library
        .list_live_event_subscribers()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut names = library
        .list_queue_names()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    for row in &catalog {
        for sub in &row.subscriptions {
            names.push(sub.queue_name().to_string());
        }
    }
    names.push(bookclerk_library::CORE_EVENTS_QUEUE.to_string());
    names.sort();
    names.dedup();
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        out.push(queue_info(&library, &catalog, &name).await?);
    }
    Ok(Json(out))
}

/// Returns one queue's counters, consumers, recent messages, and deliveries.
#[utoipa::path(
    get,
    path = "/api/queues/{name}",
    params(("name" = String, Path, description = "Queue name.")),
    responses(
        (status = 200, description = "The queue.", body = QueueDetail),
        (status = 400, description = "The queue name is invalid.", body = crate::http_error::ErrorJson),
        (status = 500, description = "The outbox could not be read.", body = crate::http_error::ErrorJson),
    ),
)]
async fn inspect_queue(
    State(state): State<Arc<AppState>>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<QueueDetail>, StatusCode> {
    if !bookclerk_library::is_valid_queue_name(&name) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let library = state.library_snapshot().await;
    let catalog = library
        .list_live_event_subscribers()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let queue = queue_info(&library, &catalog, &name).await?;
    let messages = library
        .list_domain_events_for_queue(&name, 50)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let deliveries = library
        .list_event_deliveries_for_queue(&name, None, 50)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(QueueDetail {
        queue,
        messages: messages.into_iter().map(domain_event_info).collect(),
        deliveries: deliveries.into_iter().map(event_delivery_info).collect(),
    }))
}

/// Operator projection of one delivery row.
fn event_delivery_info(d: bookclerk_library::EventDeliveryRecord) -> EventDeliveryInfo {
    EventDeliveryInfo {
        id: d.id,
        event_id: d.event_id,
        plugin_id: d.plugin_id,
        state: d.state,
        attempt_count: d.attempt_count,
        lease_generation: d.lease_generation,
        invocation_sequence: d.invocation_sequence,
        outcome: d.outcome,
        error_message: d.error_message,
        run_after: d.run_after.to_rfc3339(),
        updated_at: d.updated_at.to_rfc3339(),
        cancel_requested: d.cancel_requested,
        resource_class: d.resource_class,
        resume_pending: d.resume_pending,
        queue_name: d.queue_name.clone(),
        has_checkpoint: d.checkpoint_json.is_some(),
    }
}

/// Returns event deliveries, optionally filtered by `state`.
#[utoipa::path(
    get,
    path = "/api/events/deliveries",
    params(EventDeliveryQuery),
    responses(
        (status = 200, description = "Deliveries, newest first.", body = Vec<EventDeliveryInfo>),
        (status = 500, description = "The outbox could not be read.", body = crate::http_error::ErrorJson),
    ),
)]
async fn list_event_deliveries(
    State(state): State<Arc<AppState>>,
    Query(q): Query<EventDeliveryQuery>,
) -> Result<Json<Vec<EventDeliveryInfo>>, StatusCode> {
    let library = state.library_snapshot().await;
    let rows = library
        .list_event_deliveries(q.state.as_deref(), 200)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(rows.into_iter().map(event_delivery_info).collect()))
}

/// Re-queue a dead-lettered delivery.
#[utoipa::path(
    post,
    path = "/api/events/deliveries/{id}/retry",
    params(("id" = String, Path, description = "Delivery id.")),
    responses(
        (status = 200, description = "The delivery was queued again.", body = ActionResponse, example = json!({"ok": true, "message": "delivery b3f1c2d4-5e6f-4a7b-8c9d-0e1f2a3b4c5d:audiobookshelf re-queued", "job_id": "b3f1c2d4-5e6f-4a7b-8c9d-0e1f2a3b4c5d:audiobookshelf"})),
        (status = 404, description = "No such delivery, or it is not in a state this applies to.", body = crate::http_error::ErrorJson),
        (status = 500, description = "The outbox could not be read or written.", body = crate::http_error::ErrorJson),
    ),
)]
async fn retry_event_delivery(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<ActionResponse>, StatusCode> {
    let library = state.library_snapshot().await;
    match library.retry_dead_letter_delivery(&id).await {
        Ok(true) => {
            state.job_notify.notify_waiters();
            Ok(Json(ActionResponse {
                ok: true,
                message: format!("delivery {id} re-queued"),
                job_id: id,
                error: None,
            }))
        }
        Ok(false) => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// Acknowledge a dead-lettered delivery (no further retry).
#[utoipa::path(
    post,
    path = "/api/events/deliveries/{id}/acknowledge",
    params(("id" = String, Path, description = "Delivery id.")),
    responses(
        (status = 200, description = "The delivery was acknowledged.", body = ActionResponse, example = json!({"ok": true, "message": "delivery b3f1c2d4-5e6f-4a7b-8c9d-0e1f2a3b4c5d:audiobookshelf acknowledged", "job_id": "b3f1c2d4-5e6f-4a7b-8c9d-0e1f2a3b4c5d:audiobookshelf"})),
        (status = 404, description = "No such delivery, or it is not in a state this applies to.", body = crate::http_error::ErrorJson),
        (status = 500, description = "The outbox could not be read or written.", body = crate::http_error::ErrorJson),
    ),
)]
async fn ack_event_delivery(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<ActionResponse>, StatusCode> {
    let library = state.library_snapshot().await;
    match library.acknowledge_dead_letter_delivery(&id).await {
        Ok(true) => Ok(Json(ActionResponse {
            ok: true,
            message: format!("delivery {id} acknowledged"),
            job_id: id,
            error: None,
        })),
        Ok(false) => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// Cancel a pending delivery or request cooperative stop of a running one.
#[utoipa::path(
    post,
    path = "/api/events/deliveries/{id}/cancel",
    params(("id" = String, Path, description = "Delivery id.")),
    responses(
        (status = 200, description = "The delivery was cancelled or asked to stop.", body = ActionResponse, example = json!({"ok": true, "message": "delivery b3f1c2d4-5e6f-4a7b-8c9d-0e1f2a3b4c5d:audiobookshelf rejected", "job_id": "b3f1c2d4-5e6f-4a7b-8c9d-0e1f2a3b4c5d:audiobookshelf"})),
        (status = 404, description = "No such delivery, or it is not in a state this applies to.", body = crate::http_error::ErrorJson),
        (status = 500, description = "The outbox could not be read or written.", body = crate::http_error::ErrorJson),
    ),
)]
async fn cancel_event_delivery(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<ActionResponse>, StatusCode> {
    let library = state.library_snapshot().await;
    match library.request_event_delivery_cancel(&id).await {
        Ok(Some(row)) => {
            state.job_notify.notify_waiters();
            Ok(Json(ActionResponse {
                ok: true,
                message: format!("delivery {id} {}", row.state),
                job_id: id,
                error: None,
            }))
        }
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// Resume a suspended delivery (wake now, next claim uses resume_pending).
#[utoipa::path(
    post,
    path = "/api/events/deliveries/{id}/resume",
    params(("id" = String, Path, description = "Delivery id.")),
    responses(
        (status = 200, description = "The delivery was resumed.", body = ActionResponse, example = json!({"ok": true, "message": "delivery b3f1c2d4-5e6f-4a7b-8c9d-0e1f2a3b4c5d:audiobookshelf resumed", "job_id": "b3f1c2d4-5e6f-4a7b-8c9d-0e1f2a3b4c5d:audiobookshelf"})),
        (status = 404, description = "No such delivery, or it is not in a state this applies to.", body = crate::http_error::ErrorJson),
        (status = 500, description = "The outbox could not be read or written.", body = crate::http_error::ErrorJson),
    ),
)]
async fn resume_event_delivery(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<ActionResponse>, StatusCode> {
    let library = state.library_snapshot().await;
    match library.resume_event_delivery(&id).await {
        Ok(true) => {
            state.job_notify.notify_waiters();
            Ok(Json(ActionResponse {
                ok: true,
                message: format!("delivery {id} resumed"),
                job_id: id,
                error: None,
            }))
        }
        Ok(false) => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// Cancel a pending job or request cooperative stop of a running job.
#[utoipa::path(
    post,
    path = "/api/jobs/{id}/cancel",
    params(("id" = String, Path, description = "Job id.")),
    responses(
        (status = 200, description = "The job was cancelled or asked to stop.", body = ActionResponse, example = json!({"ok": true, "message": "job scan-7d0c1e2f-3a4b-4c5d-8e6f-9a0b1c2d3e4f cancelled", "job_id": "scan-7d0c1e2f-3a4b-4c5d-8e6f-9a0b1c2d3e4f"})),
        (status = 404, description = "No such job.", body = crate::http_error::ErrorJson),
        (status = 500, description = "The job could not be updated.", body = crate::http_error::ErrorJson),
    ),
)]
async fn cancel_job(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<ActionResponse>, StatusCode> {
    let library = state.library_snapshot().await;
    match library.request_job_cancel(&id).await {
        Ok(Some(job)) => Ok(Json(ActionResponse {
            ok: true,
            message: format!("job {id} {}", job.state.as_str()),
            job_id: id,
            error: None,
        })),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// Deprecated alias of `GET /api/status`.
#[utoipa::path(
    get,
    path = "/status",
    operation_id = "legacy_status",
    responses(
        (status = 200, description = "Library counts and daemon state.", body = StatusResponse),
        (status = 500, description = "The library could not be read.", body = crate::http_error::ErrorJson),
    ),
)]
#[deprecated(note = "legacy root alias; call the `/api/...` route instead")]
async fn legacy_status(state: State<Arc<AppState>>) -> Result<Json<StatusResponse>, StatusCode> {
    status(state).await
}

/// Deprecated alias of `POST /api/library/scan`.
#[utoipa::path(
    post,
    path = "/scan",
    operation_id = "legacy_scan",
    request_body = Option<ScanRequest>,
    responses(
        (status = 200, description = "The scan jobs were accepted; see `POST /api/library/scan` for the other answers.", body = ScanPlanResponse, example = json!({"ok": true, "message": "scan job scan-7d0c1e2f-3a4b-4c5d-8e6f-9a0b1c2d3e4f accepted", "job_id": "scan-7d0c1e2f-3a4b-4c5d-8e6f-9a0b1c2d3e4f", "job_ids": ["scan-7d0c1e2f-3a4b-4c5d-8e6f-9a0b1c2d3e4f"]})),
    ),
)]
#[deprecated(note = "legacy root alias; call the `/api/...` route instead")]
async fn legacy_scan(
    state: State<Arc<AppState>>,
    requester: Option<axum::Extension<ScanRequester>>,
    body: Option<Json<ScanRequest>>,
) -> Response {
    trigger_scan(state, requester, body).await
}

/// Deprecated alias of `POST /api/library/acquire`.
#[utoipa::path(
    post,
    path = "/acquire",
    operation_id = "legacy_acquire",
    request_body = Option<AcquireRequestBody>,
    responses(
        (status = 200, description = "The acquire jobs were accepted; see `POST /api/library/acquire` for the other answers.", body = ActionResponse, example = json!({"ok": true, "message": "acquire job acquire-3f9c2b7e-1d4a-4c8e-9b2f-6a7d8e9f0a1b accepted", "job_id": "acquire-3f9c2b7e-1d4a-4c8e-9b2f-6a7d8e9f0a1b"})),
    ),
)]
#[deprecated(note = "legacy root alias; call the `/api/...` route instead")]
async fn legacy_acquire(
    state: State<Arc<AppState>>,
    requester: Option<axum::Extension<ScanRequester>>,
    body: Option<Json<AcquireRequestBody>>,
) -> Response {
    trigger_acquire(state, requester, body).await
}

/// Deprecated alias of `GET /api/jobs`.
#[utoipa::path(
    get,
    path = "/jobs",
    operation_id = "legacy_list_jobs",
    responses(
        (status = 200, description = "Jobs, newest first.", body = Vec<JobInfo>),
        (status = 500, description = "The job table could not be read.", body = crate::http_error::ErrorJson),
    ),
)]
#[deprecated(note = "legacy root alias; call the `/api/...` route instead")]
async fn legacy_list_jobs(state: State<Arc<AppState>>) -> Result<Json<Vec<JobInfo>>, StatusCode> {
    list_jobs(state).await
}

/// Deprecated alias of `POST /api/jobs/{id}/cancel`.
#[utoipa::path(
    post,
    path = "/jobs/{id}/cancel",
    operation_id = "legacy_cancel_job",
    params(("id" = String, Path, description = "Job id.")),
    responses(
        (status = 200, description = "The job was cancelled or asked to stop.", body = ActionResponse, example = json!({"ok": true, "message": "job scan-7d0c1e2f-3a4b-4c5d-8e6f-9a0b1c2d3e4f cancelled", "job_id": "scan-7d0c1e2f-3a4b-4c5d-8e6f-9a0b1c2d3e4f"})),
        (status = 404, description = "No such job.", body = crate::http_error::ErrorJson),
    ),
)]
#[deprecated(note = "legacy root alias; call the `/api/...` route instead")]
async fn legacy_cancel_job(
    state: State<Arc<AppState>>,
    id: AxumPath<String>,
) -> Result<Json<ActionResponse>, StatusCode> {
    cancel_job(state, id).await
}

/// Enqueues a remote integration library scan; 404 if missing, 400 if unsupported.
#[utoipa::path(
    post,
    path = "/integrations/{id}/scan",
    params(("id" = String, Path, description = "Integration instance id.")),
    request_body = Option<IntegrationScanRequest>,
    responses(
        (status = 200, description = "The job was accepted.", body = ActionResponse, example = json!({"ok": true, "message": "integration_scan job integration_scan-9e8d7c6b-5a4f-4e3d-8c2b-1a0f9e8d7c6b accepted", "job_id": "integration_scan-9e8d7c6b-5a4f-4e3d-8c2b-1a0f9e8d7c6b"})),
        (status = 409, description = "The same job is already pending or running, or no node can run it.", body = ActionResponse, example = json!({"ok": false, "message": "integration_scan already pending or running", "job_id": "integration_scan-7d0c1e2f-3a4b-4c5d-8e6f-9a0b1c2d3e4f", "error": "conflict"})),
        (status = 429, description = "The job queue is full.", body = ActionResponse, example = json!({"ok": false, "message": "job queue is full", "job_id": "", "error": "queue_full"})),
        (status = 503, description = "Job admission is paused.", body = ActionResponse, example = json!({"ok": false, "message": "job admission paused (database reload in progress)", "job_id": "", "error": "admission_paused"})),
        (status = 500, description = "The job could not be queued.", body = ActionResponse, example = json!({"ok": false, "message": "ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection", "job_id": "", "error": "internal_error"})),
        (status = 404, description = "No integration with this id.", body = crate::http_error::ErrorJson),
        (status = 400, description = "The integration cannot scan a library.", body = crate::http_error::ErrorJson),
    ),
)]
async fn trigger_integration_scan(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    body: Option<Json<IntegrationScanRequest>>,
) -> Response {
    let force = body.and_then(|Json(b)| b.force).unwrap_or(false);
    {
        let integrations = state.integrations.read().await;
        let Some(integration) = integrations.get(&id) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        if !integration.supports_library_scan() {
            return StatusCode::BAD_REQUEST.into_response();
        }
    }
    match enqueue_integration_scan(state, id, force, JobTrigger::Api).await {
        Ok(admit) => admit_response("integration_scan", admit),
        Err(err) => enqueue_error_response(err),
    }
}

/// Lists or searches library titles with optional account/status filters and pagination.
#[utoipa::path(
    get,
    path = "/api/library/books",
    params(BooksQuery),
    responses(
        (status = 200, description = "A page of titles.", body = BooksResponse),
        (status = 400, description = "A filter is invalid."),
        (status = 500, description = "The library could not be read.", body = crate::http_error::ErrorJson),
    ),
)]
async fn list_books(
    State(state): State<Arc<AppState>>,
    _headers: HeaderMap,
    Query(query): Query<BooksQuery>,
) -> Result<Json<BooksResponse>, (StatusCode, Json<crate::http_error::ErrorJson>)> {
    let library = state.library_snapshot().await;
    let limit = query.limit.unwrap_or(40).clamp(1, 500);
    let offset = query.offset.unwrap_or(0);
    let status = query
        .status
        .as_deref()
        .and_then(AcquireStatus::parse)
        .map(AcquireStatus::as_str);
    let account = query.account.as_deref().filter(|value| !value.is_empty());

    // Authenticated callers (operator or user) share the full library for
    // browsing. Account *linking* and acquire remain capability-gated; store
    // connect is User-only.
    //
    // Without a query, filtering, sorting and paging run in SQL. With one,
    // the node's search index matches, sorts by title, pages and counts,
    // so no hit cap applies; only the page's uuids are hydrated from SQL.
    // Account and status are read from SQL, not the index, so a filtered
    // search takes every hit and lets SQL filter and page them.

    let page = if let Some(q) = query.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let index_dir = state.config.read().await.paths().search_index_dir.clone();
        // Offloaded: Tantivy query work would otherwise block this request task.
        if account.is_none() && status.is_none() {
            let hits = SearchEngine::open_and_search_page(index_dir, q.to_string(), offset, limit)
                .await
                .map_err(books_query_error)?;
            let mut page = library
                .list_books_by_uuid_page(&hits.uuids, None, None, limit as u64, 0)
                .await
                .map_err(books_query_error)?;
            page.total = hits.total;
            page
        } else {
            let uuids = SearchEngine::open_and_match_uuids(index_dir, q.to_string())
                .await
                .map_err(books_query_error)?;
            library
                .list_books_by_uuid_page(&uuids, account, status, limit as u64, offset as u64)
                .await
                .map_err(books_query_error)?
        }
    } else {
        library
            .list_books_filtered_page(account, status, limit as u64, offset as u64)
            .await
            .map_err(books_query_error)?
    };
    let mut books = page.books;
    for book in &mut books {
        state
            .covers
            .decorate(book.cover_url.as_deref(), &mut book.cover_path);
    }
    Ok(Json(BooksResponse {
        books,
        total: page.total,
        limit,
        offset,
    }))
}

/// Logs one library-books failure and returns the branded internal-error body.
fn books_query_error(
    err: impl std::fmt::Display,
) -> (StatusCode, Json<crate::http_error::ErrorJson>) {
    tracing::error!(error = %err, "GET /api/library/books failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(crate::http_error::ErrorJson::for_status(
            StatusCode::INTERNAL_SERVER_ERROR,
        )),
    )
}

/// Returns one title by UUID, or 404 when it is absent.
#[utoipa::path(
    get,
    path = "/api/library/books/{uuid}",
    params(("uuid" = String, Path, description = "Title UUID.")),
    responses(
        (status = 200, description = "The title.", body = bookclerk_library::BookRecord),
        (status = 404, description = "No such title.", body = crate::http_error::ErrorJson),
        (status = 500, description = "The library could not be read.", body = crate::http_error::ErrorJson),
    ),
)]
async fn get_book(
    State(state): State<Arc<AppState>>,
    AxumPath(uuid): AxumPath<String>,
) -> Result<Json<BookRecord>, StatusCode> {
    let library = state.library_snapshot().await;
    let mut book = library
        .get_book_by_uuid(&uuid)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    state
        .covers
        .decorate(book.cover_url.as_deref(), &mut book.cover_path);
    Ok(Json(book))
}

/// Serves the JPEG cover sidecar from the deployed local destination, or 404
/// when no local destination runs on this node.
#[utoipa::path(
    get,
    path = "/api/library/books/{uuid}/cover",
    params(("uuid" = String, Path, description = "Title UUID.")),
    responses(
        (status = 200, description = "The cover.", content_type = "image/jpeg"),
        (status = 404, description = "No such title, or it has no cover.", body = crate::http_error::ErrorJson),
    ),
)]
async fn get_book_cover(
    State(state): State<Arc<AppState>>,
    AxumPath(uuid): AxumPath<String>,
) -> Result<Response, StatusCode> {
    // Without a local destination there is no sidecar to serve, so answer
    // before reading the library.
    let local = state
        .destinations
        .read()
        .await
        .local()
        .ok_or(StatusCode::NOT_FOUND)?;
    let library = state.library_snapshot().await;
    let book = library
        .get_book_by_uuid(&uuid)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let storage_key = book.storage_key.as_deref().ok_or(StatusCode::NOT_FOUND)?;
    let cover_key = sidecar_key(storage_key, "jpg");
    let bytes = local
        .get(&cover_key)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "image/jpeg")],
        bytes,
    )
        .into_response())
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
/// Wishlist / title-request create body from Discover or the portal.
struct CreateRequestBody {
    /// Required title text; empty values are rejected with 400.
    #[schema(example = "The Left Hand of Darkness")]
    title: String,
    /// Optional author list as a single display string.
    #[schema(example = "Ursula K. Le Guin")]
    authors: Option<String>,
    /// Optional Audible ASIN used for work-key identity.
    #[schema(example = "B002V0QK4C")]
    asin: Option<String>,
    /// Optional ISBN used for work-key identity.
    #[schema(example = "9780441478125")]
    isbn: Option<String>,
    /// Optional operator or member note stored on the request.
    #[schema(example = "Book club pick for March")]
    notes: Option<String>,
    /// Known storefront editions to persist as `title_request_sources`.
    #[serde(default)]
    store_editions: Vec<bookclerk_discover::StoreEdition>,
    /// Optional priced storefront links snapshotted at wishlist time.
    #[serde(default)]
    purchase_hints: Vec<bookclerk_discover::PurchaseHint>,
    /// Optional cover image URL snapshotted onto the request.
    #[schema(example = "https://images.example.com/covers/9780441478125.jpg")]
    cover_url: Option<String>,
    /// Optional plot/summary text persisted with the request.
    #[schema(
        example = "An envoy to the planet Gethen must win the trust of a people who have no fixed gender."
    )]
    description: Option<String>,
    /// Optional subtitle stored on the request and source rows.
    #[schema(example = "A Hainish Novel")]
    subtitle: Option<String>,
    /// Optional narrator list as a single display string.
    #[schema(example = "George Guidall")]
    narrators: Option<String>,
    /// Optional series name.
    #[schema(example = "Hainish Cycle")]
    series: Option<String>,
    /// Optional series position as a display string.
    #[schema(example = "4")]
    series_index: Option<String>,
    /// Optional publisher name.
    #[schema(example = "Blackstone Publishing")]
    publisher: Option<String>,
    /// Optional runtime in minutes.
    #[schema(example = 560)]
    length_minutes: Option<i64>,
    /// Optional publication date as a display string.
    #[schema(example = "2005-06-21")]
    published_at: Option<String>,
    /// Optional genre list as a single display string.
    #[schema(example = "Science Fiction, Classics")]
    genres: Option<String>,
    /// Optional content language code.
    #[schema(example = "en")]
    language: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
/// Query string for multi-store Discover catalog search.
struct CatalogSearchQuery {
    /// Search text; queries shorter than two characters return an empty page.
    /// Rejected above 256 UTF-8 bytes as a downstream catalog-search limit
    /// (the HTTP query extractor has already materialized `q`).
    q: Option<String>,
    /// Alias for [`Self::page_size`] (typeahead / legacy).
    limit: Option<usize>,
    /// Page size (1–48); falls back to `limit` then 24.
    page_size: Option<usize>,
    /// Opaque cursor from a previous [`bookclerk_discover::CatalogSearchPage`].
    cursor: Option<String>,
    /// `relevance` / `popularity` / `rating` / `title` / `author` / `price` / `length`.
    sort: Option<String>,
    /// `asc` / `desc` (defaults per sort when omitted).
    sort_dir: Option<String>,
    /// Storefront region code (default `us`).
    region: Option<String>,
    /// Optional facet scope: `author` / `narrator` / `series` / `genre`.
    field: Option<String>,
    /// Preferred / default content language (BCP-47 primary). When `language`
    /// is omitted and `all_languages` is not set, applied as a hard include.
    lang: Option<String>,
    /// Comma-separated language codes to include (`en`, `zh`, …).
    language: Option<String>,
    /// When true, do not hard-filter by language (soft prefer via `lang` only).
    all_languages: Option<bool>,
    /// Comma-separated include filters (OR within a kind).
    author: Option<String>,
    /// Comma-separated narrator include filters.
    narrator: Option<String>,
    /// Comma-separated series include filters.
    series: Option<String>,
    /// Comma-separated genre include filters.
    genre: Option<String>,
    /// Comma-separated store ids to include.
    source: Option<String>,
    /// Comma-separated store ids to exclude.
    exclude_source: Option<String>,
    /// Comma-separated narrator substrings to exclude (e.g. `Virtual Voice`).
    exclude_narrator: Option<String>,
    /// Minimum overall rating (0–5); missing ratings still pass.
    min_rating: Option<f64>,
    /// Minimum runtime in minutes; values ≤ 0 are ignored.
    min_length_minutes: Option<i64>,
    /// Maximum runtime in minutes; values ≤ 0 are ignored.
    max_length_minutes: Option<i64>,
}

/// Returns the caller's stored Discover shelves; never builds them on the request.
///
/// Shelves are ranked by queued `discover_candidates` and `discover_feed`
/// jobs. This route reads the stored feed, drops the caller's hidden shelves,
/// and queues a fetch or ranking when the stored data is missing or stale:
/// `state` is `preparing` (no shelves yet), `refreshing` (stored shelves,
/// newer ones queued) or `ready`.
#[utoipa::path(
    get,
    path = "/api/discover/recommendations",
    responses(
        (status = 200, description = "The caller's stored Discover feed.", body = bookclerk_discover::DiscoverFeed),
        (status = 401, description = "The caller has no portal identity.", body = String, content_type = "text/plain", example = json!("Unauthorized")),
        (status = 500, description = "The lookup failed.", body = String, content_type = "text/plain", example = json!("ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection")),
    ),
)]
async fn discover_recommendations(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<bookclerk_discover::DiscoverFeed>, (StatusCode, String)> {
    let library = state.library_snapshot().await;

    // Nothing to recommend from: answer at once, queue nothing.
    let has_books = library.count_books(None).await.map_err(internal_err)? > 0;
    let has_open_requests = !library
        .list_title_requests(Some(RequestStatus::Open))
        .await
        .map_err(internal_err)?
        .is_empty();
    if !has_books && !has_open_requests {
        return Ok(Json(bookclerk_discover::DiscoverFeed {
            shelves: Vec::new(),
            shelf_kinds: bookclerk_discover::shelf_kind_catalog(),
            state: Some(bookclerk_discover::FeedState::Ready),
            computed_at: None,
        }));
    }

    let (subject_key, identity_id) = auth::prefs_subject_for_caller(&state, &headers)
        .await
        .map_err(|status| (status, status.canonical_reason().unwrap_or("error").into()))?;
    let subject = bookclerk_library::DiscoverSubject::parse(&subject_key)
        .unwrap_or(bookclerk_library::DiscoverSubject::Operator);
    let disabled_shelves = library
        .get_user_preferences_or_default(&subject_key, identity_id)
        .await
        .map(|p| p.disabled_shelves)
        .unwrap_or_default();

    let read = bookclerk_discover::stored_subject_feed(&library, &subject, &disabled_shelves)
        .await
        .map_err(internal_err)?;
    // A feed that does not decode reads as `None` above and counts as
    // missing, so a ranking replaces it.
    let newest = library
        .newest_discover_candidate_fetch(&subject)
        .await
        .map_err(internal_err)?;
    let refresh = crate::discover_jobs::refresh_needed(read.as_ref(), newest, chrono::Utc::now());
    crate::discover_jobs::queue_refresh(&state, &subject, refresh).await;
    // A fetch or ranking queued by a taste event or the Refresh button is
    // also a rebuild the page should show.
    let updating = refresh != crate::discover_jobs::FeedRefresh::None
        || crate::discover_jobs::subject_refresh_active(&library, &subject)
            .await
            .map_err(internal_err)?;
    let mut feed = match read {
        Some(stored) => {
            let mut feed = stored.feed;
            feed.state = Some(if updating {
                bookclerk_discover::FeedState::Refreshing
            } else {
                bookclerk_discover::FeedState::Ready
            });
            feed.computed_at = Some(stored.computed_at.to_rfc3339());
            feed
        }
        None => bookclerk_discover::DiscoverFeed {
            shelves: Vec::new(),
            shelf_kinds: bookclerk_discover::shelf_kind_catalog(),
            state: Some(bookclerk_discover::FeedState::Preparing),
            computed_at: None,
        },
    };
    if feed.shelf_kinds.is_empty() {
        feed.shelf_kinds = bookclerk_discover::shelf_kind_catalog();
    }
    let files = crate::profile::files_dir(&state).await;
    for shelf in &mut feed.shelves {
        for item in &mut shelf.items {
            hydrate_queue_wishers(&mut item.wishers, files.as_deref(), &state.covers);
            state
                .covers
                .decorate(item.cover_url.as_deref(), &mut item.cover_path);
        }
    }
    crate::covers::prefetch(
        &state,
        feed.shelves
            .iter()
            .flat_map(|shelf| shelf.items.iter().filter_map(|item| item.cover_url.clone())),
    );
    Ok(Json(feed))
}

/// Resolves storefront price/availability for one title, forcing server-side preferred stores.
#[utoipa::path(
    post,
    path = "/api/discover/purchase-hints",
    request_body = bookclerk_discover::PurchaseHintsQuery,
    responses(
        (status = 200, description = "Prices and availability.", body = bookclerk_discover::PurchaseHintsResponse),
        (status = 400, description = "The query is malformed or too large.", body = String, content_type = "text/plain", example = json!("title, asin, isbn, or candidate_product_id is required")),
    ),
)]
async fn discover_purchase_hints(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(mut body): Json<bookclerk_discover::PurchaseHintsQuery>,
) -> Result<Json<bookclerk_discover::PurchaseHintsResponse>, (StatusCode, String)> {
    validate_purchase_hints_query(&body)?;
    // Always derive linked stores server-side (ignore client-supplied overrides).
    body.preferred_sources = preferred_sources_for_caller(&state, &headers).await;
    let sources = state.sources.read().await;
    let response = bookclerk_discover::resolve_purchase_hints(&sources, &body)
        .await
        .map_err(internal_err)?;
    Ok(Json(response))
}

#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
/// Body for batched purchase-hint lookups (max 24 queries).
struct PurchaseHintsBatchBody {
    #[serde(default)]
    /// Per-title purchase-hint queries in this batch.
    queries: Vec<bookclerk_discover::PurchaseHintsQuery>,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
/// Parallel purchase-hint results aligned with the request queries.
struct PurchaseHintsBatchResponse {
    /// One purchase-hint response per query, in request order.
    results: Vec<bookclerk_discover::PurchaseHintsResponse>,
}

/// Resolves up to 24 purchase-hint queries with caller-derived preferred stores.
#[utoipa::path(
    post,
    path = "/api/discover/purchase-hints/batch",
    request_body = PurchaseHintsBatchBody,
    responses(
        (status = 200, description = "One result per query, in order.", body = PurchaseHintsBatchResponse),
        (status = 400, description = "The query is malformed or too large.", body = String, content_type = "text/plain", example = json!("at most 24 purchase-hint queries per batch")),
        (status = 500, description = "A lookup task was cancelled before it answered.", body = String, content_type = "text/plain", example = json!("purchase-hints batch task cancelled")),
    ),
)]
async fn discover_purchase_hints_batch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<PurchaseHintsBatchBody>,
) -> Result<Json<PurchaseHintsBatchResponse>, (StatusCode, String)> {
    const MAX_PURCHASE_HINT_QUERIES: usize = 24;
    let n = body.queries.len();
    if n > MAX_PURCHASE_HINT_QUERIES {
        return Err((
            StatusCode::BAD_REQUEST,
            "at most 24 purchase-hint queries per batch".into(),
        ));
    }
    let preferred = preferred_sources_for_caller(&state, &headers).await;
    // Cap is enforced above; `.min` keeps the allocation size visibly bounded for analyzers.
    let mut queries = Vec::with_capacity(n.min(MAX_PURCHASE_HINT_QUERIES));
    for mut q in body.queries {
        validate_purchase_hints_query(&q)?;
        q.preferred_sources = preferred.clone();
        queries.push(q);
    }
    let resolved = {
        let sources = state.sources.read().await;
        bookclerk_discover::resolve_purchase_hints_batch(&sources, &queries, 4).await
    };
    let mut results = Vec::with_capacity(resolved.len().min(MAX_PURCHASE_HINT_QUERIES));
    for item in resolved {
        results.push(item.map_err(internal_err)?);
    }
    Ok(Json(PurchaseHintsBatchResponse { results }))
}

/// Rejects a purchase-hint query that has no title, ASIN, ISBN, or product id.
fn validate_purchase_hints_query(
    body: &bookclerk_discover::PurchaseHintsQuery,
) -> Result<(), (StatusCode, String)> {
    if body.title.trim().is_empty()
        && body.asin.as_deref().unwrap_or("").trim().is_empty()
        && body.isbn.as_deref().unwrap_or("").trim().is_empty()
        && body
            .candidate_product_id
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "title, asin, isbn, or candidate_product_id is required".into(),
        ));
    }
    Ok(())
}

/// Resolves Audnexus-style title metadata for one query.
#[utoipa::path(
    post,
    path = "/api/discover/title-meta",
    request_body = bookclerk_discover::TitleMetaQuery,
    responses(
        (status = 200, description = "The metadata, or null when nothing matched.", body = Option<bookclerk_discover::TitleMeta>),
        (status = 400, description = "The query is malformed or too large.", body = String, content_type = "text/plain", example = json!("title, asin, or isbn is required")),
    ),
)]
async fn discover_title_meta(
    State(state): State<Arc<AppState>>,
    Json(body): Json<bookclerk_discover::TitleMetaQuery>,
) -> Result<Json<Option<bookclerk_discover::TitleMeta>>, (StatusCode, String)> {
    validate_title_meta_query(&body)?;
    let providers: Arc<dyn bookclerk_enrich::MetadataProviders> =
        Arc::new(state.integrations.read().await.clone());
    let sources = state.sources.read().await;
    let mut meta = bookclerk_discover::resolve_title_meta(&body, providers, Some(&sources))
        .await
        .map_err(internal_err)?;
    if let Some(meta) = meta.as_mut() {
        state
            .covers
            .decorate(meta.cover_url.as_deref(), &mut meta.cover_path);
    }
    Ok(Json(meta))
}

/// Fetches Audnexus reviews for an ASIN; rejects an empty ASIN with 400.
#[utoipa::path(
    post,
    path = "/api/discover/title-reviews",
    request_body = bookclerk_discover::TitleReviewsQuery,
    responses(
        (status = 200, description = "A page of reviews.", body = bookclerk_discover::TitleReviewsPage),
        (status = 400, description = "The query is malformed or too large.", body = String, content_type = "text/plain", example = json!("asin is required")),
    ),
)]
async fn discover_title_reviews(
    Json(body): Json<bookclerk_discover::TitleReviewsQuery>,
) -> Result<Json<bookclerk_discover::TitleReviewsPage>, (StatusCode, String)> {
    if body.asin.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "asin is required".into()));
    }
    let page = bookclerk_discover::resolve_title_reviews(&body)
        .await
        .map_err(internal_err)?;
    Ok(Json(page))
}

#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
/// Body for batched title-metadata lookups (max 24 queries).
struct TitleMetaBatchBody {
    #[serde(default)]
    /// Per-title metadata queries in this batch.
    queries: Vec<bookclerk_discover::TitleMetaQuery>,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
/// Title-metadata results aligned with the request; failed items are `null`.
struct TitleMetaBatchResponse {
    /// One optional metadata payload per query; enrichment errors become `None`.
    results: Vec<Option<bookclerk_discover::TitleMeta>>,
}

/// Resolves up to 24 title-metadata queries without failing the whole batch on one error.
#[utoipa::path(
    post,
    path = "/api/discover/title-meta/batch",
    request_body = TitleMetaBatchBody,
    responses(
        (status = 200, description = "One result per query, in order.", body = TitleMetaBatchResponse),
        (status = 400, description = "The query is malformed or too large.", body = String, content_type = "text/plain", example = json!("at most 24 title-meta queries per batch")),
    ),
)]
async fn discover_title_meta_batch(
    State(state): State<Arc<AppState>>,
    Json(body): Json<TitleMetaBatchBody>,
) -> Result<Json<TitleMetaBatchResponse>, (StatusCode, String)> {
    if body.queries.len() > 24 {
        return Err((
            StatusCode::BAD_REQUEST,
            "at most 24 title-meta queries per batch".into(),
        ));
    }
    for q in &body.queries {
        validate_title_meta_query(q)?;
    }
    let resolved = {
        let providers: Arc<dyn bookclerk_enrich::MetadataProviders> =
            Arc::new(state.integrations.read().await.clone());
        let sources = state.sources.read().await;
        bookclerk_discover::resolve_title_meta_batch(&body.queries, 4, providers, Some(&sources))
            .await
    };
    // Per-query enrichment failures must not 500 the whole batch — Discover
    // search already has hits; missing meta is optional.
    let results = resolved
        .into_iter()
        .map(|item| match item {
            Ok(meta) => meta,
            Err(err) => {
                tracing::debug!(error = %err, "title-meta batch item failed");
                None
            }
        })
        .collect();
    let mut results: Vec<Option<bookclerk_discover::TitleMeta>> = results;
    for meta in results.iter_mut().flatten() {
        state
            .covers
            .decorate(meta.cover_url.as_deref(), &mut meta.cover_path);
    }
    Ok(Json(TitleMetaBatchResponse { results }))
}

/// Rejects a title-metadata query that has no title, ASIN, or ISBN.
fn validate_title_meta_query(
    body: &bookclerk_discover::TitleMetaQuery,
) -> Result<(), (StatusCode, String)> {
    if body.title.trim().is_empty()
        && body.asin.as_deref().unwrap_or("").trim().is_empty()
        && body.isbn.as_deref().unwrap_or("").trim().is_empty()
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "title, asin, or isbn is required".into(),
        ));
    }
    Ok(())
}

/// Storefronts the caller is associated with (portal links, or all operator accounts).
async fn preferred_sources_for_caller(state: &AppState, headers: &HeaderMap) -> Vec<String> {
    let library = state.library_snapshot().await;
    if let Some(identity) = auth::caller_portal_identity(state, headers).await {
        return library
            .list_account_links(identity.id)
            .await
            .map(|links| {
                let mut sources: Vec<String> = links
                    .into_iter()
                    .map(|l| l.source.to_ascii_lowercase())
                    .collect();
                sources.sort();
                sources.dedup();
                sources
            })
            .unwrap_or_default();
    }
    library
        .list_accounts()
        .await
        .map(|accounts| {
            let mut sources: Vec<String> = accounts
                .into_iter()
                .map(|a| a.source.to_ascii_lowercase())
                .collect();
            sources.sort();
            sources.dedup();
            sources
        })
        .unwrap_or_default()
}

/// Runs a multi-store catalog search with a 15s wall budget and empty-query short circuit.
#[utoipa::path(
    get,
    path = "/api/discover/search",
    params(CatalogSearchQuery),
    responses(
        (status = 200, description = "A page of catalog results.", body = bookclerk_discover::CatalogSearchPage),
        (status = 400, description = "The query is malformed or too large.", body = String, content_type = "text/plain", example = json!("catalog query must be 256 bytes or fewer")),
        (status = 504, description = "The stores did not answer within the search budget.", body = String, content_type = "text/plain", example = json!("catalog search timed out")),
    ),
)]
async fn discover_catalog_search(
    State(state): State<Arc<AppState>>,
    Query(q): Query<CatalogSearchQuery>,
) -> Result<Json<bookclerk_discover::CatalogSearchPage>, (StatusCode, String)> {
    // Wall budget for parallel multi-store search (each store capped ~7s;
    // optional Libro page enrich ~3.5s; over-fetch may need a few rounds).
    const CATALOG_SEARCH_TIMEOUT: Duration = Duration::from_secs(15);
    // Downstream catalog-search limit (not an HTTP-boundary allocation cap;
    // `Query` has already deserialized `q`).
    const CATALOG_QUERY_MAX: usize = 256;
    let query = q.q.unwrap_or_default();
    if query.len() > CATALOG_QUERY_MAX {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("catalog query must be {CATALOG_QUERY_MAX} bytes or fewer"),
        ));
    }
    if query.trim().len() < 2 {
        return Ok(Json(bookclerk_discover::CatalogSearchPage {
            items: Vec::new(),
            page_size: 0,
            has_more: false,
            next_cursor: None,
            sort: String::from("relevance"),
            sort_dir: String::from("desc"),
        }));
    }
    let region = q.region.unwrap_or_else(|| String::from("us"));
    let page_size = q.page_size.or(q.limit).unwrap_or(24).clamp(1, 48);
    let sort = q
        .sort
        .as_deref()
        .map(bookclerk_discover::CatalogSearchSort::from_wire)
        .unwrap_or_default();
    let sort_dir = q
        .sort_dir
        .as_deref()
        .map(bookclerk_discover::CatalogSortDir::from_wire)
        .unwrap_or_else(|| bookclerk_discover::CatalogSortDir::default_for_sort(sort));
    let field = q
        .field
        .as_deref()
        .and_then(bookclerk_discover::CatalogSearchField::from_wire);
    let filters = bookclerk_discover::CatalogSearchFilters {
        authors: split_csv_query(q.author.as_deref()),
        narrators: split_csv_query(q.narrator.as_deref()),
        series: split_csv_query(q.series.as_deref()),
        genres: split_csv_query(q.genre.as_deref()),
        sources: split_csv_query(q.source.as_deref()),
        exclude_sources: split_csv_query(q.exclude_source.as_deref()),
        languages: split_csv_query(q.language.as_deref()),
        exclude_narrators: split_csv_query(q.exclude_narrator.as_deref()),
        min_rating: q.min_rating.filter(|r| *r > 0.0),
        min_length_minutes: q.min_length_minutes.filter(|n| *n > 0),
        max_length_minutes: q.max_length_minutes.filter(|n| *n > 0),
    };
    let sources = state.sources.read().await;
    let page = timeout(
        CATALOG_SEARCH_TIMEOUT,
        bookclerk_discover::catalog_search_page(
            &sources,
            bookclerk_discover::CatalogSearchPageOpts {
                query: &query,
                region: &region,
                page_size,
                cursor: q.cursor.as_deref(),
                sort,
                sort_dir,
                field,
                language: q.lang.as_deref(),
                all_languages: q.all_languages.unwrap_or(false),
                filters,
            },
        ),
    )
    .await
    .map_err(|_| {
        tracing::warn!(
            query_len = query.len(),
            %region,
            page_size,
            "catalog search timed out"
        );
        (
            StatusCode::GATEWAY_TIMEOUT,
            "catalog search timed out".into(),
        )
    })?
    .map_err(internal_err)?;
    let mut page = page;
    for hit in &mut page.items {
        state
            .covers
            .decorate(hit.cover_url.as_deref(), &mut hit.cover_path);
    }
    // Search results are what users scroll first: warm their thumbnails
    // before the browser asks.
    crate::covers::prefetch(
        &state,
        page.items.iter().filter_map(|hit| hit.cover_url.clone()),
    );
    Ok(Json(page))
}

/// Splits a comma-separated query parameter into trimmed non-empty tokens.
fn split_csv_query(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Lists open wishlist rows for the caller (portal identity or operator-owned).
#[utoipa::path(
    get,
    path = "/api/wishlist",
    responses(
        (status = 200, description = "The caller's open requests.", body = Vec<bookclerk_library::TitleRequestRecord>),
        (status = 401, description = "No signed-in caller."),
        (status = 500, description = "The request could not be read or written.", body = String, content_type = "text/plain", example = json!("ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection")),
    ),
)]
async fn list_wishlist(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<TitleRequestRecord>>, (StatusCode, String)> {
    let identity_id = auth::caller_portal_identity(&state, &headers)
        .await
        .map(|identity| identity.id);
    let library = state.library_snapshot().await;
    let mut rows = library
        .list_wishlist(identity_id)
        .await
        .map_err(internal_err)?;
    for row in &mut rows {
        state
            .covers
            .decorate(row.cover_url.as_deref(), &mut row.cover_path);
    }
    Ok(Json(rows))
}

/// Creates a wishlist / title-request row for the authenticated caller.
#[utoipa::path(
    post,
    path = "/api/wishlist",
    request_body = CreateRequestBody,
    responses(
        (status = 200, description = "The new request.", body = bookclerk_library::TitleRequestRecord),
        (status = 400, description = "The title is missing or invalid.", body = String, content_type = "text/plain", example = json!("title is required")),
        (status = 401, description = "No signed-in caller."),
        (status = 500, description = "The request could not be read or written.", body = String, content_type = "text/plain", example = json!("ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection")),
    ),
)]
async fn create_wishlist(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<CreateRequestBody>,
) -> Result<Json<TitleRequestRecord>, (StatusCode, String)> {
    let Json(mut row) = create_request_inner(&state, &headers, body).await?;
    state
        .covers
        .decorate(row.cover_url.as_deref(), &mut row.cover_path);
    Ok(Json(row))
}

/// Cancels an open wishlist item the caller owns; others get 403.
#[utoipa::path(
    delete,
    path = "/api/wishlist/{uuid}",
    params(("uuid" = String, Path, description = "Request UUID.")),
    responses(
        (status = 200, description = "The cancelled request.", body = bookclerk_library::TitleRequestRecord),
        (status = 403, description = "The request belongs to someone else.", body = String, content_type = "text/plain", example = json!("not your wishlist item")),
        (status = 404, description = "No such request.", body = String, content_type = "text/plain", example = json!("wishlist item not found: 0b7e4c1d-2a3f-4e5d-9c8b-7a6f5e4d3c2b")),
        (status = 401, description = "No signed-in caller."),
        (status = 500, description = "The request could not be read or written.", body = String, content_type = "text/plain", example = json!("ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection")),
        (status = 400, description = "Only open wishlist items can be removed.", body = String, content_type = "text/plain", example = json!("only open wishlist items can be removed")),
    ),
)]
async fn delete_wishlist(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(uuid): AxumPath<String>,
) -> Result<Json<TitleRequestRecord>, (StatusCode, String)> {
    let library = state.library_snapshot().await;
    let row = library
        .get_title_request_by_uuid(&uuid)
        .await
        .map_err(internal_err)?
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("wishlist item not found: {uuid}"),
        ))?;

    if row.status != RequestStatus::Open {
        return Err((
            StatusCode::BAD_REQUEST,
            "only open wishlist items can be removed".into(),
        ));
    }
    let portal = auth::caller_portal_identity(&state, &headers).await;
    match portal {
        Some(identity) => {
            if row.identity_id != Some(identity.id) {
                return Err((StatusCode::FORBIDDEN, "not your wishlist item".into()));
            }
        }
        None => {
            // Operator token: only un-wishlist operator-owned rows.
            if row.identity_id.is_some() {
                return Err((StatusCode::FORBIDDEN, "not your wishlist item".into()));
            }
        }
    }

    library
        .update_title_request_status(&uuid, RequestStatus::Cancelled, None)
        .await
        .map_err(internal_err)?;
    let mut row = library
        .get_title_request_by_uuid(&uuid)
        .await
        .map_err(internal_err)?
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("wishlist item not found: {uuid}"),
        ))?;
    state
        .covers
        .decorate(row.cover_url.as_deref(), &mut row.cover_path);
    Ok(Json(row))
}

/// Marks whether each wisher has an uploaded avatar under `$BOOKCLERK_FILES_DIR`,
/// and signs the same-origin copy of each identity picture.
fn hydrate_queue_wishers(
    wishers: &mut [QueueWisher],
    files: Option<&Path>,
    covers: &crate::covers::CoverService,
) {
    for wisher in wishers {
        wisher.picture_path = wisher
            .picture_url
            .as_deref()
            .and_then(|url| covers.thumb_src(url));
        if let Some(id) = wisher.user_id {
            wisher.has_avatar = files.is_some_and(|d| crate::profile::avatar_exists(d, id));
            wisher.avatar_source = Some(crate::profile::avatar_source_wire(
                wisher.avatar_source.as_deref(),
            ));
        }
    }
}

/// Reads the shared open-request queue from its stored ranking.
///
/// The `discover_request_queue` job ranks the queue against household taste
/// when a taste event says it may have moved; this route joins that stored
/// ranking with the open requests and the live wish counts, so it reads no
/// books, listening history or embedder. When an entry has no stored ranking
/// yet, or the ranking is older than a day, the route queues a pass and
/// answers with what it has.
#[utoipa::path(
    get,
    path = "/api/request-queue",
    responses(
        (status = 200, description = "Open requests, ranked.", body = Vec<bookclerk_discover::RankedQueueEntry>),
        (status = 500, description = "The queue could not be read.", body = String, content_type = "text/plain", example = json!("ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection")),
    ),
)]
pub(crate) async fn list_request_queue(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<bookclerk_discover::RankedQueueEntry>>, (StatusCode, String)> {
    let library = state.library_snapshot().await;
    let queue = bookclerk_discover::read_request_queue(&library)
        .await
        .map_err(internal_err)?;
    if queue.stale {
        // Folds into a pass already waiting; a failure leaves the next read
        // or taste event to queue it.
        if let Err(err) = crate::request_queue_job::enqueue_request_queue_rank(
            &state,
            JobTrigger::Scheduler,
            std::time::Duration::ZERO,
        )
        .await
        {
            tracing::warn!(error = %err, "could not queue the request queue ranking");
        }
    }
    let mut rows = queue.entries;
    let files = crate::profile::files_dir(&state).await;
    for row in &mut rows {
        hydrate_queue_wishers(&mut row.wishers, files.as_deref(), &state.covers);
        state
            .covers
            .decorate(row.cover_url.as_deref(), &mut row.cover_path);
    }
    Ok(Json(rows))
}

/// Marketplace a wish's store editions are looked up in.
const WISH_REGION: &str = "us";

/// Total time a new wish spends asking the stores, hints and edition lookups
/// together.
///
/// It stays well under the route's budget (`api_timeout_for_path`, 8 s for
/// `/api/wishlist`) so the wish is always saved: when the stores are slow the
/// wish keeps what arrived in time and the browser's own rows.
const WISH_ENRICH_BUDGET: Duration = Duration::from_secs(5);

/// Inserts a title request after requiring a non-empty title.
///
/// The server asks each store about the editions the wish names and places
/// their answers in the shared listing pool; the wish is for the work the
/// first such listing resolved to, and otherwise for the work its own text
/// resolves to.
async fn create_request_inner(
    state: &AppState,
    headers: &HeaderMap,
    body: CreateRequestBody,
) -> Result<Json<TitleRequestRecord>, (StatusCode, String)> {
    if body.title.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "title is required".into()));
    }
    let identity_id = auth::caller_portal_identity(state, headers)
        .await
        .map(|identity| identity.id);
    let library = state.library_snapshot().await;

    // Live prices and further storefronts, from the stores themselves.
    let hint_query = bookclerk_discover::PurchaseHintsQuery {
        title: body.title.clone(),
        authors: body.authors.clone(),
        asin: body.asin.clone(),
        isbn: body.isbn.clone(),
        candidate_source: None,
        candidate_product_id: None,
        store_editions: body.store_editions.clone(),
        region: Some(String::from(WISH_REGION)),
        preferred_sources: Vec::new(),
    };
    let mut sources = wishlist_sources_from_body(&body);
    // One deadline covers every store call, leaving the rest of the route's
    // budget to save the wish.
    let deadline = tokio::time::Instant::now() + WISH_ENRICH_BUDGET;
    let pooled = {
        let registry = state.sources.read().await;
        let resolve = bookclerk_discover::resolve_purchase_hints(&registry, &hint_query);
        let hints = match tokio::time::timeout_at(deadline, resolve).await {
            Ok(Ok(resolved)) => resolved.hints,
            Ok(Err(err)) => {
                tracing::debug!(error = %err, "wishlist purchase-hint resolve soft-failed");
                Vec::new()
            }
            Err(_) => {
                tracing::debug!("wishlist purchase-hint resolve ran out of time");
                Vec::new()
            }
        };
        for hint in &hints {
            sources.push(source_from_purchase_hint(hint, &body));
        }
        // The server asks each store about the editions the wish names and
        // keeps only the stores' answers in the shared pool; the browser's
        // text and prices stay on the wish's own rows.
        let editions: Vec<(String, String)> = sources
            .iter()
            .map(|s| (s.source.clone(), s.product_id.clone()))
            .collect();
        let pool = bookclerk_discover::pool_wish_editions(
            &library,
            &registry,
            &editions,
            &hints,
            WISH_REGION,
        );
        match tokio::time::timeout_at(deadline, pool).await {
            Ok(pooled) => pooled,
            Err(_) => {
                // Each pool write is its own atomic batch, so a lookup cut
                // off here leaves only whole store answers; the wish then
                // names no listing and resolves by its own text.
                tracing::debug!("wishlist edition lookup ran out of time");
                Vec::new()
            }
        }
    };
    for source in &mut sources {
        let source_id = source.source.trim().to_ascii_lowercase();
        let product_id = source.product_id.trim();
        source.listing_id = pooled
            .iter()
            .find(|p| p.source == source_id && p.product_id == product_id)
            .map(|p| p.listing_id.clone());
    }
    // The wish is for the work of the first pooled edition it names, as the
    // pool resolved it; a wish naming none falls back to its own text.
    let work = pooled.iter().find_map(|p| p.work.clone());

    let mut row = library
        .create_title_request_for(
            &NewTitleRequest {
                uuid: None,
                identity_id,
                title: body.title.clone(),
                authors: body.authors.clone(),
                asin: body.asin.clone(),
                isbn: body.isbn.clone(),
                notes: body.notes.clone(),
                status: RequestStatus::Open,
                resolved_book_uuid: None,
                cover_url: body.cover_url.clone(),
            },
            work.as_ref(),
        )
        .await
        .map_err(|err| match err {
            // A title over the listing limits names no work.
            bookclerk_library::LibraryError::Invalid(message) => (StatusCode::BAD_REQUEST, message),
            other => internal_err(other),
        })?;

    if !sources.is_empty() {
        row = library
            .upsert_title_request_sources(row.id, &sources)
            .await
            .map_err(internal_err)?;
    }
    Ok(Json(row))
}

/// Builds persistable store-edition rows from editions, purchase hints, or ASIN fallback.
fn wishlist_sources_from_body(body: &CreateRequestBody) -> Vec<NewTitleRequestSource> {
    let mut out: Vec<NewTitleRequestSource> = Vec::new();
    for ed in &body.store_editions {
        let source = ed.source.trim().to_ascii_lowercase();
        let product_id = ed.product_id.trim();
        if source.is_empty() || product_id.is_empty() {
            continue;
        }
        out.push(NewTitleRequestSource {
            source,
            product_id: product_id.to_string(),
            title: Some(body.title.clone()),
            subtitle: body.subtitle.clone(),
            authors: body.authors.clone(),
            narrators: body.narrators.clone(),
            series: body.series.clone(),
            series_index: body.series_index.clone(),
            asin: body.asin.clone(),
            isbn: body.isbn.clone(),
            description: body.description.clone(),
            publisher: body.publisher.clone(),
            length_minutes: body.length_minutes,
            published_at: body.published_at.clone(),
            categories: body.genres.clone(),
            language: body.language.clone(),
            cover_url: body.cover_url.clone(),
            ..Default::default()
        });
    }
    for hint in &body.purchase_hints {
        out.push(source_from_purchase_hint(hint, body));
    }
    // If the client sent bib fields but no editions, keep a soft identity row
    // so description still persists under a stable key when resolve finds stores.
    if out.is_empty()
        && body
            .description
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some()
    {
        if let Some(asin) = body
            .asin
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            out.push(NewTitleRequestSource {
                source: String::from("audible"),
                product_id: asin.to_string(),
                title: Some(body.title.clone()),
                subtitle: body.subtitle.clone(),
                authors: body.authors.clone(),
                narrators: body.narrators.clone(),
                series: body.series.clone(),
                series_index: body.series_index.clone(),
                asin: body.asin.clone(),
                isbn: body.isbn.clone(),
                description: body.description.clone(),
                publisher: body.publisher.clone(),
                length_minutes: body.length_minutes,
                published_at: body.published_at.clone(),
                categories: body.genres.clone(),
                language: body.language.clone(),
                cover_url: body.cover_url.clone(),
                ..Default::default()
            });
        }
    }
    out
}

/// Maps one purchase hint onto a `title_request_sources` row, filling bib gaps from the body.
fn source_from_purchase_hint(
    hint: &bookclerk_discover::PurchaseHint,
    body: &CreateRequestBody,
) -> NewTitleRequestSource {
    NewTitleRequestSource {
        source: hint.source.trim().to_ascii_lowercase(),
        product_id: hint.product_id.trim().to_string(),
        title: hint.title.clone().or_else(|| Some(body.title.clone())),
        subtitle: body.subtitle.clone(),
        authors: body.authors.clone(),
        narrators: body.narrators.clone(),
        series: body.series.clone(),
        series_index: body.series_index.clone(),
        asin: body.asin.clone(),
        isbn: body.isbn.clone(),
        description: body.description.clone(),
        publisher: body.publisher.clone(),
        length_minutes: body.length_minutes,
        published_at: body.published_at.clone(),
        categories: body.genres.clone(),
        language: body.language.clone(),
        cover_url: body.cover_url.clone(),
        url: hint.url.clone(),
        price_cents: hint.price_cents,
        currency: hint.currency.clone(),
        price_label: hint.price_label.clone(),
        list_price_cents: hint.list_price_cents,
        list_price_label: hint.list_price_label.clone(),
        member_price_cents: hint.member_price_cents,
        member_price_label: hint.member_price_label.clone(),
        listing_id: None,
    }
}

/// Enqueues a listening-progress sync; 400 when no capable integrations are loaded.
#[utoipa::path(
    post,
    path = "/api/discover/sync-listening",
    responses(
        (status = 200, description = "The job was accepted.", body = ActionResponse, example = json!({"ok": true, "message": "listen_sync job listen_sync-2b1a0f9e-8d7c-4b6a-9f5e-4d3c2b1a0f9e accepted", "job_id": "listen_sync-2b1a0f9e-8d7c-4b6a-9f5e-4d3c2b1a0f9e"})),
        (status = 409, description = "The same job is already pending or running, or no node can run it.", body = ActionResponse, example = json!({"ok": false, "message": "listen_sync already pending or running", "job_id": "listen_sync-7d0c1e2f-3a4b-4c5d-8e6f-9a0b1c2d3e4f", "error": "conflict"})),
        (status = 429, description = "The job queue is full.", body = ActionResponse, example = json!({"ok": false, "message": "job queue is full", "job_id": "", "error": "queue_full"})),
        (status = 503, description = "Job admission is paused.", body = ActionResponse, example = json!({"ok": false, "message": "job admission paused (database reload in progress)", "job_id": "", "error": "admission_paused"})),
        (status = 500, description = "The job could not be queued.", body = ActionResponse, example = json!({"ok": false, "message": "ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection", "job_id": "", "error": "internal_error"})),
        (status = 400, description = "No listening-capable integration is enabled.", body = String, content_type = "text/plain", example = json!("no listening-capable integrations are enabled")),
    ),
)]
async fn sync_listening(State(state): State<Arc<AppState>>) -> Response {
    if state
        .integrations
        .read()
        .await
        .listening_sync_providers()
        .is_empty()
    {
        return (
            StatusCode::BAD_REQUEST,
            "no listening-capable integrations are enabled",
        )
            .into_response();
    }
    match enqueue_listen_sync(state, JobTrigger::Api).await {
        Ok(admit) => admit_response("listen_sync", admit),
        Err(err) => enqueue_error_response(err),
    }
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// Normalized Discover/library preferences for the authenticated subject.
struct PreferencesResponse {
    /// Landing view id (`library`, `discover`, …) after normalization.
    #[schema(example = "library")]
    default_view: String,
    /// Lowercased Discover shelf ids the user has hidden.
    #[schema(example = json!(["chirp_deals", "from_store"]))]
    disabled_shelves: Vec<String>,
    /// Discover sort key (`relevance`, `popularity`, …).
    #[schema(example = "relevance")]
    discover_sort: String,
    /// Discover sort direction (`asc` or `desc`).
    #[schema(example = "desc")]
    discover_sort_dir: String,
    /// `null` = use browser language on the client.
    #[schema(example = "en")]
    discover_language: Option<String>,
    /// Lowercased store ids hidden from Discover results.
    #[schema(example = json!(["chirp"]))]
    discover_excluded_sources: Vec<String>,
    /// Appearance preference (`system`, `light`, or `dark`).
    #[schema(example = "system")]
    theme: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
/// Partial preferences update; omitted fields keep their stored values.
struct PatchPreferencesBody {
    /// Replacement landing view when set.
    #[schema(example = "discover")]
    default_view: Option<String>,
    /// Replacement hidden-shelf list when set.
    #[schema(example = json!(["chirp_deals", "from_store"]))]
    disabled_shelves: Option<Vec<String>>,
    /// Replacement Discover sort key when set.
    #[schema(example = "popularity")]
    discover_sort: Option<String>,
    /// Replacement Discover sort direction when set.
    #[schema(example = "desc")]
    discover_sort_dir: Option<String>,
    /// Present + value sets language; present + `null` clears to browser default;
    /// omitted leaves unchanged.
    #[serde(default, deserialize_with = "deserialize_patch_opt_string")]
    #[schema(example = "de")]
    discover_language: Option<Option<String>>,
    /// Replacement excluded-store list when set.
    #[schema(example = json!(["chirp"]))]
    discover_excluded_sources: Option<Vec<String>>,
    /// Replacement appearance preference when set (`system`, `light`, `dark`).
    #[schema(example = "dark")]
    theme: Option<String>,
}

/// Treats a present JSON value (including `null`) as an explicit language patch.
fn deserialize_patch_opt_string<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::<String>::deserialize(deserializer)?))
}

/// Loads preferences for the caller with a 3s timeout, creating defaults when missing.
#[utoipa::path(
    get,
    path = "/api/preferences",
    responses(
        (status = 200, description = "The caller's preferences.", body = PreferencesResponse),
        (status = 401, description = "No signed-in caller.", body = String, content_type = "text/plain", example = json!("Unauthorized")),
        (status = 500, description = "The request could not be read or written.", body = String, content_type = "text/plain", example = json!("ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection")),
        (status = 504, description = "The preferences row did not load in time.", body = String, content_type = "text/plain", example = json!("preferences lookup timed out")),
    ),
)]
async fn get_preferences(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<PreferencesResponse>, (StatusCode, String)> {
    const PREFERENCES_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
    let (subject_key, identity_id) = auth::prefs_subject_for_caller(&state, &headers)
        .await
        .map_err(|status| (status, status.canonical_reason().unwrap_or("error").into()))?;
    let library = state.library_snapshot().await;
    let prefs = timeout(
        PREFERENCES_TIMEOUT,
        library.get_user_preferences_or_default(&subject_key, identity_id),
    )
    .await
    .map_err(|_| {
        (
            StatusCode::GATEWAY_TIMEOUT,
            "preferences lookup timed out".into(),
        )
    })?
    .map_err(internal_err)?;
    Ok(Json(preferences_response(&prefs)))
}

/// Merges a preferences PATCH onto the caller’s stored row and writes it back.
#[utoipa::path(
    patch,
    path = "/api/preferences",
    request_body = PatchPreferencesBody,
    responses(
        (status = 200, description = "The preferences after the change.", body = PreferencesResponse),
        (status = 400, description = "The body is not syntactically valid JSON.", body = String, content_type = "text/plain", example = json!("Failed to parse the request body as JSON: expected value at line 1 column 10")),
        (status = 401, description = "No signed-in caller.", body = String, content_type = "text/plain", example = json!("Unauthorized")),
        (status = 415, description = "The request did not declare a JSON body.", body = String, content_type = "text/plain", example = json!("Expected request with `Content-Type: application/json`")),
        (status = 422, description = "The JSON does not fit the patch (a wrong type or an unknown shape).", body = String, content_type = "text/plain", example = json!("Failed to deserialize the JSON body into the target type: theme: invalid type: integer `3`, expected a string at line 1 column 11")),
        (status = 500, description = "The request could not be read or written.", body = String, content_type = "text/plain", example = json!("ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection")),
        (status = 504, description = "The preferences row did not load or save in time.", body = String, content_type = "text/plain", example = json!("preferences update timed out")),
    ),
)]
async fn patch_preferences(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<PatchPreferencesBody>,
) -> Result<Json<PreferencesResponse>, (StatusCode, String)> {
    const PREFERENCES_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
    let (subject_key, identity_id) = auth::prefs_subject_for_caller(&state, &headers)
        .await
        .map_err(|status| (status, status.canonical_reason().unwrap_or("error").into()))?;
    let library = state.library_snapshot().await;
    let current = timeout(
        PREFERENCES_TIMEOUT,
        library.get_user_preferences_or_default(&subject_key, identity_id),
    )
    .await
    .map_err(|_| {
        (
            StatusCode::GATEWAY_TIMEOUT,
            "preferences lookup timed out".into(),
        )
    })?
    .map_err(internal_err)?;

    let default_view = body
        .default_view
        .as_deref()
        .map(auth::normalize_default_view)
        .unwrap_or_else(|| auth::normalize_default_view(&current.default_view));

    let disabled_shelves = body
        .disabled_shelves
        .map(normalize_disabled_shelves)
        .unwrap_or(current.disabled_shelves);

    let discover_sort = body
        .discover_sort
        .as_deref()
        .map(normalize_discover_sort_pref)
        .unwrap_or_else(|| normalize_discover_sort_pref(&current.discover_sort));
    let discover_sort_dir = body
        .discover_sort_dir
        .as_deref()
        .map(normalize_discover_sort_dir_pref)
        .unwrap_or_else(|| normalize_discover_sort_dir_pref(&current.discover_sort_dir));
    let discover_language = match body.discover_language {
        Some(v) => v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
        None => current.discover_language,
    };
    let discover_excluded_sources = body
        .discover_excluded_sources
        .map(normalize_disabled_shelves)
        .unwrap_or(current.discover_excluded_sources);
    let theme = body
        .theme
        .as_deref()
        .map(bookclerk_library::normalize_theme)
        .unwrap_or_else(|| bookclerk_library::normalize_theme(&current.theme));

    let saved = timeout(
        PREFERENCES_TIMEOUT,
        library.upsert_user_preferences(
            &subject_key,
            identity_id,
            &default_view,
            &disabled_shelves,
            &discover_sort,
            &discover_sort_dir,
            discover_language.as_deref(),
            &discover_excluded_sources,
            &theme,
        ),
    )
    .await
    .map_err(|_| {
        (
            StatusCode::GATEWAY_TIMEOUT,
            "preferences update timed out".into(),
        )
    })?
    .map_err(internal_err)?;

    Ok(Json(preferences_response(&saved)))
}

/// Projects stored preferences through the same normalizers the PATCH path uses.
fn preferences_response(prefs: &bookclerk_library::UserPreferences) -> PreferencesResponse {
    PreferencesResponse {
        default_view: auth::normalize_default_view(&prefs.default_view),
        disabled_shelves: prefs.disabled_shelves.clone(),
        discover_sort: normalize_discover_sort_pref(&prefs.discover_sort),
        discover_sort_dir: normalize_discover_sort_dir_pref(&prefs.discover_sort_dir),
        discover_language: prefs.discover_language.clone(),
        discover_excluded_sources: prefs.discover_excluded_sources.clone(),
        theme: bookclerk_library::normalize_theme(&prefs.theme),
    }
}

/// Maps a sort string onto a known Discover key, defaulting unknown values to `relevance`.
fn normalize_discover_sort_pref(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "popularity" => String::from("popularity"),
        "rating" => String::from("rating"),
        "title" => String::from("title"),
        "author" => String::from("author"),
        "price" => String::from("price"),
        "length" | "runtime" => String::from("length"),
        _ => String::from("relevance"),
    }
}

/// Maps a sort-direction string to `asc` or `desc` (default `desc`).
fn normalize_discover_sort_dir_pref(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "asc" | "ascending" => String::from("asc"),
        _ => String::from("desc"),
    }
}

/// Lowercases, trims, and de-duplicates shelf or source id lists.
fn normalize_disabled_shelves(raw: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for item in raw {
        let trimmed = item.trim().to_ascii_lowercase();
        if trimmed.is_empty() {
            continue;
        }
        if !out.iter().any(|existing| existing == &trimmed) {
            out.push(trimmed);
        }
    }
    out
}

/// Maps an internal error into HTTP 500 plus the display string.
fn internal_err(err: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

#[cfg(test)]
mod tests {
    /// Asking the stores about a new wish leaves at least two seconds of the
    /// route's budget to save it.
    #[test]
    fn a_wish_asks_the_stores_within_its_route_budget() {
        let route = super::api_timeout_for_path("/api/wishlist");
        assert!(super::WISH_ENRICH_BUDGET + std::time::Duration::from_secs(2) <= route);
    }

    #[test]
    fn books_query_error_returns_the_branded_internal_error_body() {
        let (status, axum::Json(body)) = super::books_query_error("boom");
        assert_eq!(status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        let body = serde_json::to_value(body).unwrap();
        assert_eq!(body["status"], 500);
        assert_eq!(body["error"], "internal_error");
        assert_eq!(
            body["message"],
            "Something went wrong on the Bookclerk daemon."
        );
        assert_eq!(
            body,
            serde_json::to_value(crate::http_error::ErrorJson::for_status(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            ))
            .unwrap()
        );
    }

    use super::{
        allowed_setting_key, apply_database_enable_updates, build_approved_grant,
        build_database_settings_group, current_settings_snapshot,
        database_backends_requiring_grant, normalize_disabled_shelves, normalize_setting_value,
        stamp_settings_occupancy, validate_daemon_listen, validate_daemon_listen_against_auth,
        InviteLinkError, PluginGrantOverride,
    };
    use bookclerk_config::{Config, ListenAddrs};

    /// Only a streamed file PUT runs without the API deadline; the
    /// commit and every other upload request keep it.
    #[test]
    fn only_streamed_upload_puts_skip_the_api_deadline() {
        use super::is_streamed_upload;
        use axum::http::Method;
        assert!(is_streamed_upload(
            &Method::PUT,
            "/api/node/v1/uploads/u-1/files/0"
        ));
        assert!(!is_streamed_upload(
            &Method::POST,
            "/api/node/v1/uploads/u-1/commit"
        ));
        assert!(!is_streamed_upload(
            &Method::PUT,
            "/api/node/v1/uploads/u-1/commit"
        ));
        assert!(!is_streamed_upload(
            &Method::POST,
            "/api/node/v1/uploads/u-1/files/0"
        ));
        assert!(!is_streamed_upload(&Method::PUT, "/api/library/upload"));
    }

    /// An acquire whose `local` outputs share no node is a 409 `unroutable`
    /// refusal, not a 500; other admission failures keep their codes.
    #[tokio::test]
    async fn unroutable_acquire_is_conflict_not_internal_error() {
        use axum::http::StatusCode;
        use bookclerk_library::control_plane::{
            create_plugin_instance, ensure_plugin_deployment, resolve_job_requirements,
            ConfigActor, DesiredState,
        };
        use bookclerk_library::{JobKind, JobPayload, LibraryStore};

        const LOCAL: &str = "platform:bookclerk/bookclerk-plugin-destination-local";
        let store = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        );
        let operator = ConfigActor::Operator {
            id: "operator".into(),
        };
        for (name, node) in [("", "node-a"), ("second", "node-b")] {
            let instance = create_plugin_instance(&store, &operator, LOCAL, name)
                .await
                .unwrap();
            ensure_plugin_deployment(
                &store,
                &ConfigActor::Bootstrap,
                &instance.id,
                node,
                DesiredState::Present,
            )
            .await
            .unwrap();
        }
        let bound = std::collections::BTreeSet::from([LOCAL.to_string()]);
        let err =
            resolve_job_requirements(&store, JobKind::Acquire, &JobPayload::default(), &bound)
                .await
                .expect_err("no node runs both outputs");

        let response = super::enqueue_error_response(crate::jobs::requirements_error(err));
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"], "unroutable", "{body}");
        assert_eq!(body["ok"], false, "{body}");

        let other = super::enqueue_error_response(crate::jobs::requirements_error(
            bookclerk_library::LibraryError::Invalid("bad row".into()),
        ));
        assert_eq!(other.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn invite_link_error_classifies_missing_invalid_expired_redeemed() {
        use chrono::{Duration, Utc};

        use super::invite_link_error;
        use bookclerk_library::ClaimTicketRecord;

        let now = Utc::now();
        let live = ClaimTicketRecord {
            id: 1,
            token_hash: "x".into(),
            identity_id: Some(1),
            expires_at: now + Duration::hours(1),
            redeemed_at: None,
            created_by: "test".into(),
            created_at: now,
        };
        assert_eq!(
            invite_link_error(None, None, now),
            Some(InviteLinkError::MissingTicket)
        );
        assert_eq!(
            invite_link_error(Some("  "), None, now),
            Some(InviteLinkError::MissingTicket)
        );
        assert_eq!(
            invite_link_error(Some("nope"), None, now),
            Some(InviteLinkError::Invalid)
        );
        assert_eq!(invite_link_error(Some("ok"), Some(&live), now), None);
        let expired = ClaimTicketRecord {
            expires_at: now - Duration::minutes(1),
            ..live.clone()
        };
        assert_eq!(
            invite_link_error(Some("ok"), Some(&expired), now),
            Some(InviteLinkError::Expired)
        );
        let redeemed = ClaimTicketRecord {
            redeemed_at: Some(now),
            ..live
        };
        assert_eq!(
            invite_link_error(Some("ok"), Some(&redeemed), now),
            Some(InviteLinkError::AlreadyRedeemed)
        );
    }

    #[test]
    fn settings_allowlist_accepts_expected_keys() {
        assert!(allowed_setting_key("daemon.listen"));
        assert!(allowed_setting_key("daemon.auth.enabled"));
        assert!(allowed_setting_key("library.auto_acquire"));
        assert!(allowed_setting_key("library.scan_interval_minutes"));
        assert!(allowed_setting_key("jobs.max_pending"));
        assert!(allowed_setting_key("jobs.concurrency.network"));
        assert!(allowed_setting_key("events.retention_days"));
        assert!(allowed_setting_key("events.dead_letter_retention_days"));
        assert!(allowed_setting_key("events.concurrency"));
        assert!(
            !allowed_setting_key("sources.audible.region"),
            "plugin settings are edited on the plugin instance, not in config.toml"
        );
        assert!(!allowed_setting_key("sources.audible.enabled"));
        assert!(!allowed_setting_key("integrations.audiobookshelf.enabled"));
        assert!(!allowed_setting_key("output.s3.bucket"));
        assert!(allowed_setting_key("database.plugin"));
        assert!(allowed_setting_key("database.sqlite.path"));
        assert!(!allowed_setting_key("database.sqlite.path.extra"));
        assert!(allowed_setting_key("database.sqlite.enabled"));
        assert!(!allowed_setting_key("database"));
        assert!(!allowed_setting_key("database..enabled"));
        assert!(allowed_setting_key("plugins.isolation"));
        assert!(allowed_setting_key("media.isolation"));
        assert!(allowed_setting_key("plugins.jail.memory_mib"));
        assert!(allowed_setting_key("plugins.jail.cpu_cores"));
        assert!(allowed_setting_key("plugins.jail.cpu_rate_percent"));
        assert!(allowed_setting_key("plugins.jail.extra_processes"));
        assert!(!allowed_setting_key("plugins.jail"));
    }

    #[test]
    fn event_queue_status_json_uses_snake_case() {
        let json = serde_json::to_value(super::EventQueueStatus {
            pending: 1,
            running: 2,
            suspended: 3,
            dead_letter: 4,
            acked: 5,
            oldest_pending_age_secs: Some(9),
            retries_total: 6,
            suspensions_total: 7,
            dead_letters_total: 8,
            dispatch_latency_ms_avg: Some(10),
            handler_latency_ms_avg: Some(11),
        })
        .unwrap();
        assert_eq!(json["pending"], 1);
        assert_eq!(json["running"], 2);
        assert_eq!(json["suspended"], 3);
        assert_eq!(json["dead_letter"], 4);
        assert_eq!(json["acked"], 5);
        assert_eq!(json["oldest_pending_age_secs"], 9);
        assert_eq!(json["retries_total"], 6);
        assert_eq!(json["suspensions_total"], 7);
        assert_eq!(json["dead_letters_total"], 8);
        assert_eq!(json["dispatch_latency_ms_avg"], 10);
        assert_eq!(json["handler_latency_ms_avg"], 11);
    }

    #[test]
    fn settings_snapshot_includes_event_retention_keys() {
        let cfg = Config::default();
        let snapshot = current_settings_snapshot(&cfg);
        assert_eq!(
            snapshot.get("events.retention_days").map(String::as_str),
            Some("7")
        );
        assert_eq!(
            snapshot
                .get("events.dead_letter_retention_days")
                .map(String::as_str),
            Some("30")
        );
        assert_eq!(
            snapshot.get("events.concurrency").map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn settings_snapshot_includes_confinement_keys() {
        let mut cfg = Config::default();
        cfg.plugins.jail.memory_mib = Some(512);
        cfg.plugins.jail.cpu_rate_percent = Some(55);
        let snapshot = current_settings_snapshot(&cfg);
        assert_eq!(
            snapshot.get("plugins.isolation").map(String::as_str),
            Some("required")
        );
        assert_eq!(
            snapshot.get("media.isolation").map(String::as_str),
            Some("required")
        );
        assert_eq!(
            snapshot.get("plugins.jail.memory_mib").map(String::as_str),
            Some("512")
        );
        assert_eq!(
            snapshot.get("plugins.jail.cpu_cores").map(String::as_str),
            Some("0.55")
        );
        assert_eq!(
            snapshot
                .get("plugins.jail.extra_processes")
                .map(String::as_str),
            Some("2")
        );
    }

    #[test]
    fn build_approved_grant_accepts_widen_and_narrow() {
        use bookclerk_plugin_host::PluginGrant;
        use std::collections::BTreeSet;

        let baseline = PluginGrant {
            schema_version: bookclerk_plugin_host::GRANT_SCHEMA_VERSION,
            plugin_key: String::new(),
            plugin_id: "demo".into(),
            entrypoints: BTreeSet::from(["storefront".into()]),
            producers: BTreeSet::new(),
            consumers: Default::default(),
            jobs: Default::default(),
            network_mode: "outbound".into(),
            domains: BTreeSet::from(["a.example".into(), "b.example".into()]),
            manifest_domains: BTreeSet::from(["a.example".into(), "b.example".into()]),
            operator_added_domains: BTreeSet::new(),
            operator_denied_domains: BTreeSet::new(),
            bindings: BTreeSet::from(["config".into(), "secrets".into()]),
            compatibility_flags: BTreeSet::new(),
            cpu_ms: Some(30_000),
            subrequests: Some(50),
            disk_mib: Some(512),
            memory_mib: Some(512),
            cpu_rate_percent: Some(80),
            extra_processes: Some(2),
            approved_at: "2026-01-01T00:00:00Z".into(),
            ..PluginGrant::empty()
        };
        let approved = build_approved_grant(
            &baseline,
            Some(PluginGrantOverride {
                network_mode: Some("deny".into()),
                domains: Some(vec!["a.example".into()]),
                bindings: Some(vec!["config".into()]),
                compatibility_flags: None,
                cpu_ms: Some(12_000),
                subrequests: Some(10),
                disk_mib: Some(1024),
                memory_mib: Some(256),
                cpu_cores: Some(0.40),
                extra_processes: Some(1),
                ..PluginGrantOverride::default()
            }),
        )
        .expect("narrow");
        assert_eq!(approved.network_mode, "deny");
        assert_eq!(
            approved.domains.iter().cloned().collect::<Vec<_>>(),
            vec!["a.example".to_string()]
        );
        assert_eq!(approved.cpu_ms, Some(12_000));
        assert_eq!(approved.disk_mib, Some(1024));
        assert_eq!(approved.memory_mib, Some(256));
        assert_eq!(approved.cpu_rate_percent, Some(40));
        assert_eq!(approved.extra_processes, Some(1));

        let widen = build_approved_grant(
            &baseline,
            Some(PluginGrantOverride {
                network_mode: None,
                domains: Some(vec!["a.example".into(), "evil.example".into()]),
                bindings: Some(vec!["config".into(), "secrets".into()]),
                compatibility_flags: None,
                cpu_ms: Some(60_000),
                subrequests: None,
                disk_mib: Some(2048),
                memory_mib: Some(1024),
                cpu_cores: Some(0.90),
                extra_processes: Some(8),
                ..PluginGrantOverride::default()
            }),
        )
        .expect("network widen beyond baseline");
        assert!(widen.domains.contains("evil.example"));
        assert!(widen.operator_added_domains.contains("evil.example"));
        assert!(!widen.bindings.contains("oauth"));
        assert_eq!(widen.cpu_ms, Some(60_000));
        assert_eq!(widen.disk_mib, Some(2048));
        assert_eq!(widen.memory_mib, Some(1024));
        assert_eq!(widen.cpu_rate_percent, Some(90));
        assert_eq!(widen.extra_processes, Some(8));

        let steal = build_approved_grant(
            &baseline,
            Some(PluginGrantOverride {
                network_mode: None,
                domains: None,
                bindings: Some(vec!["config".into(), "secrets".into(), "oauth".into()]),
                compatibility_flags: None,
                cpu_ms: None,
                subrequests: None,
                disk_mib: None,
                memory_mib: None,
                cpu_cores: None,
                extra_processes: None,
                ..PluginGrantOverride::default()
            }),
        );
        assert!(steal.is_err(), "operator must not invent oauth binding");
    }

    #[test]
    fn own_user_agent_hosts_reach_the_approval_and_the_consent_view() {
        use bookclerk_plugin_host::PluginGrant;

        let baseline = PluginGrant {
            plugin_id: "libro".into(),
            network_mode: "outbound".into(),
            own_user_agent_hosts: Some(
                ["*.libro.fm".to_string(), "api.example.com".to_string()].into(),
            ),
            own_user_agent_reason: Some("App API.".into()),
            ..PluginGrant::empty()
        };
        let approved = build_approved_grant(
            &baseline,
            Some(PluginGrantOverride {
                own_user_agent_hosts: Some(vec!["*.libro.fm".into()]),
                ..PluginGrantOverride::default()
            }),
        )
        .expect("narrow own-agent hosts");
        assert_eq!(
            approved.own_user_agent_hosts,
            Some(["*.libro.fm".to_string()].into())
        );

        let view = serde_json::to_value(super::PluginGrantView::from_grant(&approved)).unwrap();
        assert_eq!(view["ownUserAgentHosts"], serde_json::json!(["*.libro.fm"]));
        assert_eq!(view["ownUserAgentReason"], "App API.");
        let legacy = PluginGrant {
            own_user_agent_hosts: None,
            ..baseline
        };
        let view = serde_json::to_value(super::PluginGrantView::from_grant(&legacy)).unwrap();
        assert!(view.get("ownUserAgentHosts").is_none(), "{view}");
    }

    #[test]
    fn echoing_the_redirect_default_is_not_an_operator_answer() {
        use bookclerk_plugin_host::{pending_structural, PluginGrant};

        let baseline = PluginGrant {
            plugin_id: "demo".into(),
            network_mode: "outbound".into(),
            ..PluginGrant::empty()
        };
        let echoed = build_approved_grant(
            &baseline,
            Some(PluginGrantOverride {
                allow_undeclared_public_redirects: Some(false),
                ..PluginGrantOverride::default()
            }),
        )
        .expect("echo the default");
        assert_eq!(echoed.operator_allow_undeclared_public_redirects, None);

        // A later version asking for redirects still waits for consent.
        let upgrade = PluginGrant {
            allow_undeclared_public_redirects: true,
            ..baseline.clone()
        };
        assert!(pending_structural(&echoed, &upgrade).undeclared_public_redirects);

        // Refusing what the manifest asks for is recorded as a refusal.
        let refused = build_approved_grant(
            &upgrade,
            Some(PluginGrantOverride {
                allow_undeclared_public_redirects: Some(false),
                ..PluginGrantOverride::default()
            }),
        )
        .expect("refuse redirects");
        assert_eq!(
            refused.operator_allow_undeclared_public_redirects,
            Some(false)
        );
        assert!(!pending_structural(&refused, &upgrade).undeclared_public_redirects);
    }

    #[test]
    fn listen_validation_uses_effective_auth_flag() {
        let mut cfg = Config::default();
        cfg.daemon.listen = ListenAddrs::parse_list("0.0.0.0:8787").unwrap();
        cfg.daemon.auth.enabled = true;
        assert!(validate_daemon_listen(&cfg).is_ok());
        assert!(validate_daemon_listen_against_auth(&cfg, true).is_ok());
        assert!(validate_daemon_listen_against_auth(&cfg, false).is_err());

        cfg.daemon.listen = ListenAddrs::parse_list("127.0.0.1:8787").unwrap();
        cfg.daemon.auth.enabled = false;
        assert!(validate_daemon_listen(&cfg).is_ok());
        assert!(validate_daemon_listen_against_auth(&cfg, false).is_ok());
    }

    /// The node API's TLS listener serves the web app, so with auth off it
    /// must be loopback too; with auth on any address is fine.
    #[test]
    fn listen_validation_covers_the_node_api_tls_listener() {
        let mut cfg = Config::default();
        cfg.daemon.listen = ListenAddrs::parse_list("127.0.0.1:8787").unwrap();
        cfg.daemon.node_api.mode = bookclerk_config::NodeApiMode::SelfSigned;
        cfg.daemon.node_api.tls_listen = Some("0.0.0.0:8443".into());
        assert!(validate_daemon_listen_against_auth(&cfg, true).is_ok());
        let err = validate_daemon_listen_against_auth(&cfg, false).unwrap_err();
        assert!(err.to_string().contains("tls_listen"), "{err}");
        cfg.daemon.node_api.tls_listen = Some("127.0.0.1:8443".into());
        assert!(validate_daemon_listen_against_auth(&cfg, false).is_ok());
        // Without a TLS listener of its own the address is not served.
        cfg.daemon.node_api.mode = bookclerk_config::NodeApiMode::Proxy;
        cfg.daemon.node_api.tls_listen = Some("0.0.0.0:8443".into());
        assert!(validate_daemon_listen_against_auth(&cfg, false).is_ok());
    }

    #[test]
    fn database_plugin_override_requires_grant() {
        assert_eq!(
            database_backends_requiring_grant(&[("database.plugin".into(), "d1".into())], "sqlite"),
            vec!["d1".to_string()]
        );
        assert!(database_backends_requiring_grant(
            &[("database.plugin".into(), "sqlite".into())],
            "sqlite"
        )
        .is_empty());
        assert!(database_backends_requiring_grant(
            &[("database.plugin".into(), "".into())],
            "sqlite"
        )
        .is_empty());
        assert_eq!(
            database_backends_requiring_grant(
                &[("database.postgres.enabled".into(), "true".into())],
                "sqlite"
            ),
            vec!["postgres".to_string()]
        );
        assert!(database_backends_requiring_grant(
            &[("database.sqlite.enabled".into(), "true".into())],
            "sqlite"
        )
        .is_empty());
        assert!(database_backends_requiring_grant(
            &[("database.sqlite.enabled".into(), "true".into())],
            "platform:bookclerk/bookclerk-plugin-database-sqlite"
        )
        .is_empty());
        assert_eq!(
            database_backends_requiring_grant(
                &[("database.plugin".into(), "d1".into())],
                "platform:bookclerk/bookclerk-plugin-database-sqlite"
            ),
            vec!["d1".to_string()]
        );
        assert_eq!(
            database_backends_requiring_grant(
                &[(
                    "database.plugin".into(),
                    "platform:bookclerk/bookclerk-plugin-database-sqlite".into()
                )],
                "sqlite"
            ),
            vec!["platform:bookclerk/bookclerk-plugin-database-sqlite".to_string()]
        );
    }

    #[test]
    fn database_groups_keep_their_bootstrap_settings() {
        let cfg = Config::default();

        let d1 = build_database_settings_group(&cfg, "d1", toml::Table::new());
        assert!(d1
            .settings
            .iter()
            .any(|option| option.key == "database.d1.account_id"));
        assert!(d1
            .settings
            .iter()
            .any(|option| option.key == "database.d1.database_id"));
        assert!(d1
            .settings
            .iter()
            .any(|option| option.key == "database.d1.api_base"));

        let postgres = build_database_settings_group(&cfg, "postgres", toml::Table::new());
        assert!(postgres
            .settings
            .iter()
            .any(|option| option.key == "database.postgres.url"));
        assert!(postgres
            .settings
            .iter()
            .any(|option| option.key == "database.postgres.url_file"));
    }

    #[test]
    fn normalize_setting_value_validates_core_fields() {
        assert_eq!(
            normalize_setting_value("library.scan_interval_minutes", "15").expect("int"),
            "15"
        );
        assert!(normalize_setting_value("library.scan_interval_minutes", "nope").is_err());

        assert_eq!(
            normalize_setting_value("daemon.auth.enabled", "true").expect("bool"),
            "true"
        );
        assert_eq!(
            normalize_setting_value("library.auto_acquire", "0").expect("bool"),
            "0"
        );
        assert!(normalize_setting_value("daemon.auth.enabled", "maybe").is_err());

        assert_eq!(
            normalize_setting_value("daemon.listen", " 127.0.0.1:8787 ").expect("listen"),
            "127.0.0.1:8787"
        );
        assert_eq!(
            normalize_setting_value("daemon.listen", "127.0.0.1:1,[::1]:1").expect("multi"),
            "127.0.0.1:1,[::1]:1"
        );
        assert!(normalize_setting_value("daemon.listen", "   ").is_err());
        assert!(normalize_setting_value("daemon.listen", "not-a-socket").is_err());

        assert_eq!(
            normalize_setting_value("sources.audible.region", " us ").expect("plugin"),
            "us"
        );
        assert_eq!(
            normalize_setting_value("sources.audible.enabled", "yes").expect("enabled bool"),
            "true"
        );
        assert!(normalize_setting_value("sources.audible.enabled", "sometimes").is_err());
    }

    #[test]
    fn database_enable_updates_clear_active_on_disable() {
        let mut cfg = Config::default();
        assert_eq!(cfg.database.plugin, "sqlite");
        apply_database_enable_updates(
            &mut cfg,
            &[("database.sqlite.enabled".into(), "false".into())],
            &[],
        )
        .expect("disable");
        assert_eq!(cfg.database.plugin, "");

        apply_database_enable_updates(
            &mut cfg,
            &[("database.d1.enabled".into(), "true".into())],
            &[],
        )
        .expect("enable d1");
        assert_eq!(cfg.database.plugin, "d1");

        apply_database_enable_updates(
            &mut cfg,
            &[
                ("database.d1.enabled".into(), "false".into()),
                ("database.postgres.enabled".into(), "true".into()),
            ],
            &[],
        )
        .expect("switch");
        assert_eq!(cfg.database.plugin, "postgres");
    }

    #[test]
    fn database_enabled_toggle_follows_plugin_key_occupancy() {
        let mut cfg = Config::default();
        cfg.database.plugin = "platform:bookclerk/bookclerk-plugin-database-sqlite".into();
        let sqlite = build_database_settings_group(&cfg, "sqlite", toml::Table::new());
        assert_eq!(
            sqlite
                .settings
                .iter()
                .find(|option| option.key == "database.sqlite.enabled")
                .map(|option| option.value.as_str()),
            Some("true"),
            "first-party PluginKey occupancy must still show sqlite as enabled"
        );
        let postgres = build_database_settings_group(&cfg, "postgres", toml::Table::new());
        assert_eq!(
            postgres
                .settings
                .iter()
                .find(|option| option.key == "database.postgres.enabled")
                .map(|option| option.value.as_str()),
            Some("false")
        );
    }

    #[test]
    fn database_enable_updates_clear_plugin_key_occupancy() {
        let mut cfg = Config::default();
        cfg.database.plugin = "platform:bookclerk/bookclerk-plugin-database-sqlite".into();
        apply_database_enable_updates(
            &mut cfg,
            &[("database.sqlite.enabled".into(), "false".into())],
            &[],
        )
        .expect("disable keyed sqlite");
        assert_eq!(cfg.database.plugin, "");
    }

    #[test]
    fn stamp_settings_occupancy_fail_closed_when_missing() {
        let mut cfg = Config::default();
        cfg.database.plugin = "postgres".into();
        let err = stamp_settings_occupancy(&mut cfg, &[], &["postgres".into()]).unwrap_err();
        assert!(
            err.contains("not installed") || err.contains("postgres"),
            "{err}"
        );
        assert_eq!(cfg.database.plugin, "postgres");
    }

    #[test]
    fn normalize_disabled_shelves_trims_dedupes_and_lowercases() {
        let normalized = normalize_disabled_shelves(vec![
            "  Requests ".into(),
            "requests".into(),
            " Continue-Series ".into(),
            "".into(),
            "   ".into(),
            "continue-series".into(),
        ]);
        assert_eq!(normalized, vec!["requests", "continue-series"]);
    }

    fn stage_logo_plugin(files: &std::path::Path, logo_line: &str) -> std::path::PathBuf {
        let root = files.join("plugins").join("logo_echo");
        std::fs::create_dir_all(root.join("assets")).unwrap();
        let bin = root.join("guest");
        std::fs::write(&bin, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Minimal 1x1 PNG
        let png: &[u8] = &[
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x08,
            0xd7, 0x63, 0xf8, 0xcf, 0xc0, 0x00, 0x00, 0x00, 0x03, 0x00, 0x01, 0x00, 0x05, 0xfe,
            0xd4, 0xef, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        std::fs::write(root.join("assets/logo.png"), png).unwrap();
        std::fs::write(
            root.join("plugin.toml"),
            format!(
                r#"
api_version = 3
id = "logo_echo"
version = "0.1.0"
{logo_line}
runtime = "native"
command = "./guest"
entrypoints = ["cli"]

[capabilities.network]
mode = "deny"
"#
            ),
        )
        .unwrap();
        root
    }

    /// Stages a native plugin asking for `tcp` (a TOML array of TCP grants).
    fn stage_network_plugin(files: &std::path::Path, tcp: &str) {
        let root = files.join("plugins").join("net_echo");
        std::fs::create_dir_all(&root).unwrap();
        let bin = root.join("guest");
        std::fs::write(&bin, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(
            root.join("plugin.toml"),
            format!(
                r#"
api_version = 3
id = "net_echo"
version = "0.1.0"
runtime = "native"
command = "./guest"
entrypoints = ["cli"]

[capabilities.network]
mode = "outbound"
tcp = {tcp}
"#
            ),
        )
        .unwrap();
    }

    /// An `AppState` over `cfg` with an in-memory library and auth off.
    async fn consent_test_state(cfg: Config) -> std::sync::Arc<super::AppState> {
        use bookclerk_integrations::IntegrationRegistry;
        use bookclerk_library::LibraryStore;
        use bookclerk_plugin_host::{DatabaseRegistry, DestinationRegistry};
        use bookclerk_source::SourceRegistry;
        use std::sync::Arc;
        use tokio::sync::{Mutex, Notify, RwLock, Semaphore};

        let library = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .expect("sqlite memory"),
        );
        Arc::new(super::AppState {
            config: Arc::new(RwLock::new(cfg)),
            library: Arc::new(RwLock::new(library)),
            database_registry: Arc::new(RwLock::new(DatabaseRegistry::default())),
            job_notify: Arc::new(Notify::new()),
            job_runtime: Arc::new(RwLock::new(())),
            work_lock: Mutex::new(()),
            discover_gate: Arc::new(Semaphore::new(1)),
            integrations: Arc::new(RwLock::new(IntegrationRegistry::new())),
            sources: Arc::new(RwLock::new(SourceRegistry::new())),
            destinations: Arc::new(RwLock::new(DestinationRegistry::default())),
            auth: Arc::new(RwLock::new(Arc::new(crate::auth::OperatorAuthState::new(
                String::new(),
                12,
                false,
                5,
                30,
            )))),
            reload_lock: Mutex::new(()),
            listen_reload: Arc::new(Notify::new()),
            last_bound_listen: RwLock::new(None),
            tray: RwLock::new(None),
            tray_handoff: Mutex::new(None),
            deployment_runtime: std::sync::OnceLock::new(),
            node_api_tls_bound: std::sync::OnceLock::new(),
            library_swaps: tokio::sync::watch::Sender::new(0),
            covers: Arc::new(crate::covers::CoverService::detached()),
        })
    }

    /// An upgrade that adds a TCP port is reported as pending (#700), and
    /// approving it clears that.
    #[tokio::test]
    async fn plugin_consent_reports_what_an_upgrade_adds() {
        use axum::extract::{Path, Query, State};
        use axum::Json;

        let files = tempfile::tempdir().unwrap();
        stage_network_plugin(
            files.path(),
            r#"[{ host = "db.store.example", ports = [5432] }]"#,
        );
        let cfg = Config::load(Some(files.path().to_path_buf()), None).unwrap();
        let plugin = bookclerk_plugin_host::discover_plugins(&cfg)
            .unwrap()
            .into_iter()
            .find(|p| p.manifest.id == "net_echo")
            .expect("staged plugin");
        let approved =
            bookclerk_plugin_host::consent_request(&plugin.manifest, plugin.plugin_key());
        let mut store = bookclerk_plugin_host::PluginGrantStore::default();
        store.upsert(approved);
        store.save(files.path()).unwrap();

        // The upgrade asks for another port on the same host.
        stage_network_plugin(
            files.path(),
            r#"[{ host = "db.store.example", ports = [5432, 6432] }]"#,
        );
        let state = consent_test_state(cfg).await;
        let Json(before) = super::get_plugin_consent(
            State(state.clone()),
            Path("net_echo".to_string()),
            Query(super::PluginConsentQuery::default()),
        )
        .await
        .expect("consent");
        let body = serde_json::to_value(&before).unwrap();
        assert_eq!(body["covered"], false, "{body}");
        assert_eq!(
            body["pending"]["tcp"],
            serde_json::json!([{ "host": "db.store.example", "ports": [6432] }])
        );
        assert!(body["pending"]["lines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l == "  raw TCP connect: db.store.example:[6432]"));

        let Json(after) = super::post_plugin_consent(
            State(state.clone()),
            Path("net_echo".to_string()),
            Query(super::PluginConsentQuery::default()),
            Json(super::PluginConsentApproveRequest {
                approve: true,
                grant: None,
            }),
        )
        .await
        .expect("approve");
        let body = serde_json::to_value(&after).unwrap();
        assert_eq!(body["covered"], true, "{body}");
        assert_eq!(body["pending"]["lines"], serde_json::json!([]));
    }

    fn write_logo_svg(root: &std::path::Path, svg: &str) {
        std::fs::write(root.join("assets/logo.svg"), svg.as_bytes()).unwrap();
    }

    #[test]
    fn embedded_logo_route_bytes_via_discovery() {
        let files = tempfile::tempdir().unwrap();
        stage_logo_plugin(files.path(), r#"logo = "assets/logo.png""#);
        let cfg = Config::load(Some(files.path().to_path_buf()), None).unwrap();
        let (bytes, ctype) =
            super::plugin_logo_bytes_for(&cfg, "integration", "logo_echo").expect("logo bytes");
        assert_eq!(ctype, "image/png");
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn remote_logo_url_is_not_served_by_embed_route() {
        let files = tempfile::tempdir().unwrap();
        stage_logo_plugin(
            files.path(),
            r#"logo = "https://www.google.com/s2/favicons?domain=example.com&sz=128""#,
        );
        let cfg = Config::load(Some(files.path().to_path_buf()), None).unwrap();
        let err = super::plugin_logo_bytes_for(&cfg, "integration", "logo_echo").unwrap_err();
        assert_eq!(err, axum::http::StatusCode::NOT_FOUND);
    }

    #[test]
    fn embedded_svg_logo_is_sanitized_stripping_script() {
        let files = tempfile::tempdir().unwrap();
        let root = stage_logo_plugin(files.path(), r#"logo = "assets/logo.svg""#);
        write_logo_svg(
            &root,
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10">
  <script>alert(1)</script>
  <circle cx="5" cy="5" r="4" fill="#333" onclick="alert(2)"/>
</svg>"##,
        );
        let cfg = Config::load(Some(files.path().to_path_buf()), None).unwrap();
        let (bytes, ctype) =
            super::plugin_logo_bytes_for(&cfg, "integration", "logo_echo").expect("sanitized svg");
        assert_eq!(ctype, "image/svg+xml");
        let body = String::from_utf8(bytes).expect("utf-8 svg");
        assert!(
            !body.to_ascii_lowercase().contains("<script"),
            "script element must be stripped: {body}"
        );
        assert!(
            !body.to_ascii_lowercase().contains("onclick"),
            "onclick must be stripped: {body}"
        );
        assert!(
            body.contains("circle") || body.contains("<svg"),
            "benign markup should remain: {body}"
        );
    }

    #[test]
    fn malformed_svg_logo_is_rejected_not_served_raw() {
        let files = tempfile::tempdir().unwrap();
        let root = stage_logo_plugin(files.path(), r#"logo = "assets/logo.svg""#);
        // Not well-formed XML — svg-hush must fail closed (no raw bytes).
        write_logo_svg(&root, "<svg><script>alert(1)</script><not-closed>");
        let cfg = Config::load(Some(files.path().to_path_buf()), None).unwrap();
        let err = super::plugin_logo_bytes_for(&cfg, "integration", "logo_echo").unwrap_err();
        assert_eq!(err, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn queue_info_counts_deliveries_and_lists_live_consumers() {
        let store = bookclerk_library::LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .expect("memory store"),
        );
        store
            .upsert_event_subscriber(
                "node-a",
                "path:file:///tmp/a#qecho",
                &[bookclerk_library::EventCatalogSubscription::queue("echo")],
                true,
            )
            .await
            .expect("catalog");
        store
            .upsert_event_subscriber(
                "node-a",
                "path:file:///tmp/b#disabled",
                &[bookclerk_library::EventCatalogSubscription::queue("echo")],
                false,
            )
            .await
            .expect("catalog");
        let event_id = match store
            .publish_domain_event(bookclerk_library::PublishDomainEventSpec {
                schema_version: 1,
                dedup_key: "m1".into(),
                payload: "{}".into(),
                queue_name: "echo".into(),
                ..bookclerk_library::PublishDomainEventSpec::default()
            })
            .await
            .expect("publish")
        {
            bookclerk_library::PublishDomainEventOutcome::Created { id } => id,
            other => panic!("unexpected publish outcome {other:?}"),
        };
        let catalog = store
            .list_live_event_subscribers()
            .await
            .expect("live catalog");
        let event = store
            .get_domain_event(&event_id)
            .await
            .expect("query")
            .expect("row");
        let subscribers = bookclerk_library::catalog_subscribers_for_event(&catalog, &event);
        assert_eq!(subscribers.len(), 1, "only the enabled consumer subscribes");
        store
            .dispatch_event_deliveries(&event_id, &subscribers, "op-1")
            .await
            .expect("dispatch");
        let info = super::queue_info(&store, &catalog, "echo")
            .await
            .expect("queue info");
        assert_eq!(info.name, "echo");
        assert!(!info.core);
        assert_eq!(info.pending, 1, "{info:?}");
        assert_eq!(info.running, 0);
        assert_eq!(info.acked, 0);
        assert_eq!(info.dead_letter, 0);
        assert_eq!(info.consumers, vec!["path:file:///tmp/a#qecho".to_string()]);
        let core = super::queue_info(&store, &catalog, bookclerk_library::CORE_EVENTS_QUEUE)
            .await
            .expect("core info");
        assert!(core.core);
        assert_eq!(core.pending, 0);
        assert!(core.consumers.is_empty());
    }

    #[tokio::test]
    async fn plugin_logo_http_route_serves_png() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let files = tempfile::tempdir().unwrap();
        stage_logo_plugin(files.path(), r#"logo = "assets/logo.png""#);
        let cfg = Config::load(Some(files.path().to_path_buf()), None).unwrap();

        async fn logo_handler(
            axum::extract::State(cfg): axum::extract::State<Config>,
            axum::extract::Path((kind, id)): axum::extract::Path<(String, String)>,
        ) -> Result<axum::response::Response, StatusCode> {
            let (bytes, content_type) = super::plugin_logo_bytes_for(&cfg, &kind, &id)?;
            Ok(super::plugin_logo_response(bytes, content_type))
        }

        let app = axum::Router::new()
            .route(
                "/api/plugins/{kind}/{id}/logo",
                axum::routing::get(logo_handler),
            )
            .with_state(cfg);

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/plugins/integration/logo_echo/logo")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get(axum::http::header::CONTENT_TYPE).unwrap(),
            "image/png"
        );
        assert_eq!(
            res.headers()
                .get(axum::http::header::X_CONTENT_TYPE_OPTIONS)
                .unwrap(),
            "nosniff"
        );
        assert!(res
            .headers()
            .get(axum::http::header::CONTENT_SECURITY_POLICY)
            .is_none());
        let body = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[tokio::test]
    async fn plugin_logo_http_route_serves_benign_svg_with_csp() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let files = tempfile::tempdir().unwrap();
        let root = stage_logo_plugin(files.path(), r#"logo = "assets/logo.svg""#);
        write_logo_svg(
            &root,
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 8 8">
  <rect width="8" height="8" fill="#abc"/>
</svg>"##,
        );
        let cfg = Config::load(Some(files.path().to_path_buf()), None).unwrap();

        async fn logo_handler(
            axum::extract::State(cfg): axum::extract::State<Config>,
            axum::extract::Path((kind, id)): axum::extract::Path<(String, String)>,
        ) -> Result<axum::response::Response, StatusCode> {
            let (bytes, content_type) = super::plugin_logo_bytes_for(&cfg, &kind, &id)?;
            Ok(super::plugin_logo_response(bytes, content_type))
        }

        let app = axum::Router::new()
            .route(
                "/api/plugins/{kind}/{id}/logo",
                axum::routing::get(logo_handler),
            )
            .with_state(cfg);

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/plugins/integration/logo_echo/logo")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get(axum::http::header::CONTENT_TYPE).unwrap(),
            "image/svg+xml"
        );
        assert_eq!(
            res.headers()
                .get(axum::http::header::X_CONTENT_TYPE_OPTIONS)
                .unwrap(),
            "nosniff"
        );
        assert_eq!(
            res.headers()
                .get(axum::http::header::CONTENT_SECURITY_POLICY)
                .unwrap(),
            super::SVG_LOGO_CSP
        );
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("rect") || text.contains("<svg"), "{text}");
        assert!(!text.to_ascii_lowercase().contains("<script"), "{text}");
    }

    #[tokio::test]
    async fn plugin_logo_http_route_rejects_malicious_unparseable_svg() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let files = tempfile::tempdir().unwrap();
        let root = stage_logo_plugin(files.path(), r#"logo = "assets/logo.svg""#);
        write_logo_svg(&root, "<svg><script>alert(1)</script><broken");
        let cfg = Config::load(Some(files.path().to_path_buf()), None).unwrap();

        async fn logo_handler(
            axum::extract::State(cfg): axum::extract::State<Config>,
            axum::extract::Path((kind, id)): axum::extract::Path<(String, String)>,
        ) -> Result<axum::response::Response, StatusCode> {
            let (bytes, content_type) = super::plugin_logo_bytes_for(&cfg, &kind, &id)?;
            Ok(super::plugin_logo_response(bytes, content_type))
        }

        let app = axum::Router::new()
            .route(
                "/api/plugins/{kind}/{id}/logo",
                axum::routing::get(logo_handler),
            )
            .with_state(cfg);

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/plugins/integration/logo_echo/logo")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&body);
        assert!(
            !text.contains("<script") && !text.contains("alert"),
            "must not leak raw svg: {text}"
        );
    }

    /// #116: after swapping auth-disabled → auth-enabled (as reload does before
    /// advertising a public listen), protected routes must reject anonymous calls.
    #[tokio::test]
    async fn auth_reload_enables_middleware_before_public_listen() {
        use std::sync::Arc;

        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use axum::middleware;
        use axum::routing::get;
        use axum::Router;
        use bookclerk_integrations::IntegrationRegistry;
        use bookclerk_library::LibraryStore;
        use bookclerk_plugin_host::{DatabaseRegistry, DestinationRegistry};
        use bookclerk_source::SourceRegistry;
        use tokio::sync::{Mutex, Notify, RwLock, Semaphore};
        use tower::ServiceExt;

        use crate::api::AppState;
        use crate::auth::{require_operator_auth, OperatorAuthState};

        let library = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .expect("sqlite memory"),
        );
        let mut cfg = Config::default();
        cfg.daemon.listen = ListenAddrs::parse_list("127.0.0.1:8787").unwrap();
        cfg.daemon.auth.enabled = false;

        let state = Arc::new(AppState {
            config: Arc::new(RwLock::new(cfg)),
            library: Arc::new(RwLock::new(library)),
            database_registry: Arc::new(RwLock::new(DatabaseRegistry::default())),
            job_notify: Arc::new(Notify::new()),
            job_runtime: Arc::new(RwLock::new(())),
            work_lock: Mutex::new(()),
            discover_gate: Arc::new(Semaphore::new(1)),
            integrations: Arc::new(RwLock::new(IntegrationRegistry::new())),
            sources: Arc::new(RwLock::new(SourceRegistry::new())),
            destinations: Arc::new(RwLock::new(DestinationRegistry::default())),
            auth: Arc::new(RwLock::new(Arc::new(OperatorAuthState::new(
                String::new(),
                12,
                false,
                5,
                30,
            )))),
            reload_lock: Mutex::new(()),
            listen_reload: Arc::new(Notify::new()),
            last_bound_listen: RwLock::new(None),
            tray: RwLock::new(None),
            tray_handoff: Mutex::new(None),
            deployment_runtime: std::sync::OnceLock::new(),
            node_api_tls_bound: std::sync::OnceLock::new(),
            library_swaps: tokio::sync::watch::Sender::new(0),
            covers: Arc::new(crate::covers::CoverService::detached()),
        });

        let app = Router::new()
            .route("/api/status", get(|| async { StatusCode::OK }))
            .route_layer(middleware::from_fn_with_state(
                state.clone(),
                require_operator_auth,
            ))
            .with_state(state.clone());

        let open = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(open.status(), StatusCode::OK);

        // Simulate reload publish: enable auth (and conceptually move listen public)
        // before any unauthenticated request can slip through.
        {
            let mut auth = state.auth.write().await;
            let previous = auth.clone();
            let mut next = OperatorAuthState::new("reload-token-value-001".into(), 12, true, 5, 30);
            next.install_token_grace_from(&previous, crate::auth::OPERATOR_TOKEN_GRACE);
            next.take_session_state_from(&previous).await;
            *auth = Arc::new(next);
        }
        state.config.write().await.daemon.listen = ListenAddrs::parse_list("0.0.0.0:8787").unwrap();
        state.config.write().await.daemon.auth.enabled = true;

        let denied = app
            .oneshot(
                Request::builder()
                    .uri("/api/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn scan_admit_returns_200_then_409_and_cancel() {
        use std::sync::Arc;

        use axum::body::Body;
        use axum::http::{header, Request, StatusCode};
        use axum::routing::{get, post};
        use axum::Router;
        use bookclerk_integrations::IntegrationRegistry;
        use bookclerk_library::LibraryStore;
        use bookclerk_plugin_host::{DatabaseRegistry, DestinationRegistry};
        use bookclerk_source::SourceRegistry;
        use http_body_util::BodyExt;
        use tokio::sync::{Mutex, Notify, RwLock, Semaphore};
        use tower::ServiceExt;

        use crate::api::AppState;
        use crate::auth::OperatorAuthState;

        let library = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .expect("sqlite memory"),
        );
        let mut cfg = Config::default();
        cfg.daemon.auth.enabled = false;
        cfg.jobs.max_pending = 1;

        let state = Arc::new(AppState {
            config: Arc::new(RwLock::new(cfg)),
            library: Arc::new(RwLock::new(library)),
            database_registry: Arc::new(RwLock::new(DatabaseRegistry::default())),
            job_notify: Arc::new(Notify::new()),
            job_runtime: Arc::new(RwLock::new(())),
            work_lock: Mutex::new(()),
            discover_gate: Arc::new(Semaphore::new(1)),
            integrations: Arc::new(RwLock::new(IntegrationRegistry::new())),
            sources: Arc::new(RwLock::new(SourceRegistry::new())),
            destinations: Arc::new(RwLock::new(DestinationRegistry::default())),
            auth: Arc::new(RwLock::new(Arc::new(OperatorAuthState::new(
                String::new(),
                12,
                false,
                5,
                30,
            )))),
            reload_lock: Mutex::new(()),
            listen_reload: Arc::new(Notify::new()),
            last_bound_listen: RwLock::new(None),
            tray: RwLock::new(None),
            tray_handoff: Mutex::new(None),
            deployment_runtime: std::sync::OnceLock::new(),
            node_api_tls_bound: std::sync::OnceLock::new(),
            library_swaps: tokio::sync::watch::Sender::new(0),
            covers: Arc::new(crate::covers::CoverService::detached()),
        });

        let app = Router::new()
            .route("/api/library/scan", post(super::trigger_scan))
            .route("/api/library/acquire", post(super::trigger_acquire))
            .route("/api/jobs", get(super::list_jobs))
            .route("/api/jobs/{id}/cancel", post(super::cancel_job))
            .layer(axum::Extension(super::ScanRequester::Operator))
            .with_state(state);

        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/library/scan")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let first_body = first.into_body().collect().await.unwrap().to_bytes();
        let first_json: serde_json::Value = serde_json::from_slice(&first_body).unwrap();
        let job_id = first_json["job_id"].as_str().unwrap().to_string();
        assert!(first_json["ok"].as_bool().unwrap());

        let dup = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/library/scan")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(dup.status(), StatusCode::CONFLICT);
        let dup_body = dup.into_body().collect().await.unwrap().to_bytes();
        let dup_json: serde_json::Value = serde_json::from_slice(&dup_body).unwrap();
        assert_eq!(dup_json["job_id"], job_id);
        assert_eq!(dup_json["error"], "conflict");

        let full = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/library/acquire")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(full.status(), StatusCode::TOO_MANY_REQUESTS);
        let full_json: serde_json::Value =
            serde_json::from_slice(&full.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(full_json["error"], "queue_full");

        let listed = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/jobs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(listed.status(), StatusCode::OK);
        let jobs: Vec<serde_json::Value> =
            serde_json::from_slice(&listed.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0]["id"], job_id);
        assert_eq!(jobs[0]["status"], "pending");

        let cancel = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/jobs/{job_id}/cancel"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cancel.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn scan_all_is_planned_per_account_and_needs_a_requester() {
        use std::sync::Arc;

        use axum::body::Body;
        use axum::http::{header, Request, StatusCode};
        use axum::routing::post;
        use axum::Router;
        use bookclerk_integrations::IntegrationRegistry;
        use bookclerk_library::LibraryStore;
        use bookclerk_plugin_host::{DatabaseRegistry, DestinationRegistry};
        use bookclerk_source::SourceRegistry;
        use http_body_util::BodyExt;
        use tokio::sync::{Mutex, Notify, RwLock, Semaphore};
        use tower::ServiceExt;

        use crate::api::AppState;
        use crate::auth::OperatorAuthState;

        let library = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .expect("sqlite memory"),
        );
        for (account, scan) in [("a1", true), ("a2", true), ("a3", false)] {
            library
                .upsert_account("pk", account, "us", None, scan, "test")
                .await
                .unwrap();
        }
        let member = library
            .create_user(bookclerk_library::UserRole::Member, Some("Riley"), None)
            .await
            .unwrap()
            .id;
        let mut cfg = Config::default();
        cfg.daemon.auth.enabled = false;
        cfg.jobs.max_pending = 8;
        let state = Arc::new(AppState {
            config: Arc::new(RwLock::new(cfg)),
            library: Arc::new(RwLock::new(library)),
            database_registry: Arc::new(RwLock::new(DatabaseRegistry::default())),
            job_notify: Arc::new(Notify::new()),
            job_runtime: Arc::new(RwLock::new(())),
            work_lock: Mutex::new(()),
            discover_gate: Arc::new(Semaphore::new(1)),
            integrations: Arc::new(RwLock::new(IntegrationRegistry::new())),
            sources: Arc::new(RwLock::new(SourceRegistry::new())),
            destinations: Arc::new(RwLock::new(DestinationRegistry::default())),
            auth: Arc::new(RwLock::new(Arc::new(OperatorAuthState::new(
                String::new(),
                12,
                false,
                5,
                30,
            )))),
            reload_lock: Mutex::new(()),
            listen_reload: Arc::new(Notify::new()),
            last_bound_listen: RwLock::new(None),
            tray: RwLock::new(None),
            tray_handoff: Mutex::new(None),
            deployment_runtime: std::sync::OnceLock::new(),
            node_api_tls_bound: std::sync::OnceLock::new(),
            library_swaps: tokio::sync::watch::Sender::new(0),
            covers: Arc::new(crate::covers::CoverService::detached()),
        });
        let scan = || {
            Request::builder()
                .method("POST")
                .uri("/api/library/scan")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap()
        };

        // Without the auth layer's requester the handler refuses.
        let bare = Router::new()
            .route("/api/library/scan", post(super::trigger_scan))
            .with_state(state.clone());
        let refused = bare.oneshot(scan()).await.unwrap();
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);

        let app = Router::new()
            .route("/api/library/scan", post(super::trigger_scan))
            .route("/api/library/acquire", post(super::trigger_acquire))
            .layer(axum::Extension(super::ScanRequester::Operator))
            .with_state(state.clone());
        let acquire = |body: serde_json::Value| {
            Request::builder()
                .method("POST")
                .uri("/api/library/acquire")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap()
        };
        // Each named title is its own acquire job.
        let titles = app
            .clone()
            .oneshot(acquire(serde_json::json!({ "titles": ["T1", "T2", "t1"] })))
            .await
            .unwrap();
        assert_eq!(titles.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&titles.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["job_ids"].as_array().unwrap().len(), 2, "{body}");
        // A Member may not acquire an account that is not theirs, and owning
        // nothing has nothing to acquire.
        let as_member = Router::new()
            .route("/api/library/acquire", post(super::trigger_acquire))
            .layer(axum::Extension(super::ScanRequester::User(member)))
            .with_state(state.clone());
        let refused = as_member
            .clone()
            .oneshot(acquire(serde_json::json!({ "account": "a1" })))
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        let nothing = as_member
            .oneshot(acquire(serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(nothing.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body: serde_json::Value =
            serde_json::from_slice(&nothing.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["error"], "nothing_to_acquire");

        let first = app.clone().oneshot(scan()).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&first.into_body().collect().await.unwrap().to_bytes()).unwrap();
        let ids = body["job_ids"].as_array().unwrap().clone();
        assert_eq!(ids.len(), 2, "one job per scan-enabled account: {body}");
        assert_eq!(body["job_id"], ids[0]);

        let again = app.oneshot(scan()).await.unwrap();
        assert_eq!(again.status(), StatusCode::CONFLICT);
        let body: serde_json::Value =
            serde_json::from_slice(&again.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(body["error"], "conflict");
        assert_eq!(body["job_ids"].as_array().unwrap(), &ids);
    }

    #[tokio::test]
    async fn mixed_settings_request_does_not_commit_events() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        use axum::extract::State;
        use axum::http::StatusCode;
        use axum::Json;
        use bookclerk_library::control_plane::{
            audit_count, bootstrap_control_plane, change_count, load_events, overlay_events,
            EVENTS_NAMESPACE,
        };
        use bookclerk_library::LibraryStore;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[daemon]\nlisten = \"127.0.0.1:8787\"\n\n[daemon.auth]\nenabled = true\n",
        )
        .unwrap();
        let mut cfg = bookclerk_config::Config::load(Some(dir.path().to_path_buf()), None).unwrap();
        let db = bookclerk_plugin_database_sqlite::open(&dir.path().join("library.db"))
            .await
            .unwrap();
        bookclerk_library::apply_host_schema(&db).await.unwrap();
        let store = LibraryStore::from_connection(db);
        let scratch = dir.path().join("cache");
        let session = bootstrap_control_plane(&store, dir.path(), &scratch, None, &cfg)
            .await
            .unwrap();
        overlay_events(&mut cfg, &session.events, &session.cluster_id);
        let original_retention = session.events.body.retention_days;
        let requested = if original_retention == 14 { 15 } else { 14 };
        let audits = audit_count(&store, EVENTS_NAMESPACE).await.unwrap();
        let changes = change_count(&store, EVENTS_NAMESPACE).await.unwrap();
        let toml_before = std::fs::read(dir.path().join("config.toml")).unwrap();
        let state = std::sync::Arc::new(crate::config_authority::control_plane_test_state(
            store.clone(),
            cfg,
        ));

        let err = super::patch_settings(
            State(state.clone()),
            Json(super::PatchSettingsRequest {
                settings: vec![
                    super::SettingsUpdate {
                        key: "events.retention_days".into(),
                        value: requested.to_string(),
                    },
                    super::SettingsUpdate {
                        key: "daemon.listen".into(),
                        value: "0.0.0.0:8787".into(),
                    },
                    super::SettingsUpdate {
                        key: "daemon.auth.enabled".into(),
                        value: "false".into(),
                    },
                ],
                expected_output_revision: None,
            }),
        )
        .await
        .expect_err("mixed authorities");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        use http_body_util::BodyExt;
        let bytes = err.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"], "mixed_configuration_authorities");

        let loaded = load_events(&store).await.unwrap();
        assert_eq!(loaded.revision, session.events.revision);
        assert_eq!(loaded.body.retention_days, original_retention);
        assert_eq!(audit_count(&store, EVENTS_NAMESPACE).await.unwrap(), audits);
        assert_eq!(
            change_count(&store, EVENTS_NAMESPACE).await.unwrap(),
            changes
        );
        assert_eq!(
            std::fs::read(dir.path().join("config.toml")).unwrap(),
            toml_before
        );
        let live = state.config.read().await;
        assert_eq!(live.events.retention_days, original_retention);
        assert_eq!(live.events_revision, Some(session.events.revision));
        assert!(!live.daemon.listen.join_comma().contains("0.0.0.0"));
        assert!(live.daemon.auth.enabled);
    }

    #[tokio::test]
    async fn events_only_settings_patch_commits() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        use axum::extract::State;
        use axum::Json;
        use bookclerk_library::control_plane::{
            bootstrap_control_plane, load_events, overlay_events,
        };
        use bookclerk_library::LibraryStore;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[daemon]\nlisten = \"127.0.0.1:8787\"\n\n[daemon.auth]\nenabled = true\n",
        )
        .unwrap();
        let mut cfg = bookclerk_config::Config::load(Some(dir.path().to_path_buf()), None).unwrap();
        let db = bookclerk_plugin_database_sqlite::open(&dir.path().join("library.db"))
            .await
            .unwrap();
        bookclerk_library::apply_host_schema(&db).await.unwrap();
        let store = LibraryStore::from_connection(db);
        let scratch = dir.path().join("cache");
        let session = bootstrap_control_plane(&store, dir.path(), &scratch, None, &cfg)
            .await
            .unwrap();
        overlay_events(&mut cfg, &session.events, &session.cluster_id);
        let requested = if session.events.body.retention_days == 14 {
            15
        } else {
            14
        };
        let state = std::sync::Arc::new(crate::config_authority::control_plane_test_state(
            store.clone(),
            cfg,
        ));
        let ok = super::patch_settings(
            State(state.clone()),
            Json(super::PatchSettingsRequest {
                settings: vec![super::SettingsUpdate {
                    key: "events.retention_days".into(),
                    value: requested.to_string(),
                }],
                expected_output_revision: None,
            }),
        )
        .await
        .expect("events-only patch");
        assert_eq!(
            ok.0.effective
                .get("events.retention_days")
                .map(String::as_str),
            Some(requested.to_string()).as_deref()
        );
        let loaded = load_events(&store).await.unwrap();
        assert_eq!(loaded.body.retention_days, requested);
        assert_eq!(loaded.revision, session.events.revision + 1);
        let live = state.config.read().await;
        assert_eq!(live.events.retention_days, requested);
        assert_eq!(live.events_revision, Some(loaded.revision));
    }

    #[tokio::test]
    async fn cache_settings_patch_commits_to_the_database_only() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        use axum::extract::State;
        use axum::Json;
        use bookclerk_library::control_plane::{bootstrap_control_plane, load_cache_settings};
        use bookclerk_library::LibraryStore;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[daemon]\nlisten = \"127.0.0.1:8787\"\n",
        )
        .unwrap();
        let cfg = bookclerk_config::Config::load(Some(dir.path().to_path_buf()), None).unwrap();
        let db = bookclerk_plugin_database_sqlite::open(&dir.path().join("library.db"))
            .await
            .unwrap();
        bookclerk_library::apply_host_schema(&db).await.unwrap();
        let store = LibraryStore::from_connection(db);
        bootstrap_control_plane(&store, dir.path(), &dir.path().join("cache"), None, &cfg)
            .await
            .unwrap();
        let toml_before = std::fs::read(dir.path().join("config.toml")).unwrap();
        let state = std::sync::Arc::new(crate::config_authority::control_plane_test_state(
            store.clone(),
            cfg,
        ));
        let patch = |settings: Vec<(&str, &str)>| super::PatchSettingsRequest {
            settings: settings
                .into_iter()
                .map(|(key, value)| super::SettingsUpdate {
                    key: key.into(),
                    value: value.into(),
                })
                .collect(),
            expected_output_revision: None,
        };

        let before = super::get_settings(State(state.clone())).await.unwrap();
        assert_eq!(
            before
                .0
                .settings
                .get("cache.budget_bytes")
                .map(String::as_str),
            Some("1073741824")
        );

        let ok = super::patch_settings(
            State(state.clone()),
            Json(patch(vec![
                ("cache.unviewed_days", "30"),
                ("cache.budget_bytes", "536870912"),
            ])),
        )
        .await
        .expect("cache-only patch");
        assert_eq!(
            ok.0.effective
                .get("cache.unviewed_days")
                .map(String::as_str),
            Some("30")
        );
        let stored = load_cache_settings(&store).await.unwrap().expect("stored");
        assert_eq!(stored.body.unviewed_days, 30);
        assert_eq!(stored.body.budget_bytes, 512 << 20);
        assert_eq!(
            std::fs::read(dir.path().join("config.toml")).unwrap(),
            toml_before,
            "cache settings never reach config.toml"
        );

        for bad in [
            patch(vec![("cache.budget_bytes", "12")]),
            patch(vec![
                ("cache.unviewed_days", "31"),
                ("events.concurrency", "2"),
            ]),
        ] {
            let err = super::patch_settings(State(state.clone()), Json(bad))
                .await
                .expect_err("rejected");
            assert_eq!(err.status(), axum::http::StatusCode::BAD_REQUEST);
        }
        let unchanged = load_cache_settings(&store).await.unwrap().expect("stored");
        assert_eq!(unchanged.revision, stored.revision);
    }

    #[tokio::test]
    async fn output_settings_patch_commits_and_rejects_mixed_or_invalid_requests() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        use axum::extract::State;
        use axum::http::StatusCode;
        use axum::Json;
        use bookclerk_library::control_plane::{
            audit_count, bootstrap_control_plane, load_output, overlay_core_documents,
            OUTPUT_NAMESPACE,
        };
        use bookclerk_library::LibraryStore;

        let dir = tempfile::tempdir().unwrap();
        // The TOML seed: imported once, then ignored.
        std::fs::write(
            dir.path().join("config.toml"),
            "[daemon]\nlisten = \"127.0.0.1:8787\"\n\n[daemon.auth]\nenabled = true\n\n\
             [output]\nwidevine = true\nnaming_profile = \"classic\"\n",
        )
        .unwrap();
        let mut cfg = bookclerk_config::Config::load(Some(dir.path().to_path_buf()), None).unwrap();
        let db = bookclerk_plugin_database_sqlite::open(&dir.path().join("library.db"))
            .await
            .unwrap();
        bookclerk_library::apply_host_schema(&db).await.unwrap();
        let store = LibraryStore::from_connection(db);
        let scratch = dir.path().join("cache");
        let session = bootstrap_control_plane(&store, dir.path(), &scratch, None, &cfg)
            .await
            .unwrap();
        assert!(session.output.body.widevine);
        overlay_core_documents(&mut cfg, &session);
        let state = std::sync::Arc::new(crate::config_authority::control_plane_test_state(
            store.clone(),
            cfg,
        ));
        let patch = |settings: Vec<(&str, &str)>| {
            super::patch_settings(
                State(state.clone()),
                Json(super::PatchSettingsRequest {
                    settings: settings
                        .into_iter()
                        .map(|(key, value)| super::SettingsUpdate {
                            key: key.into(),
                            value: value.into(),
                        })
                        .collect(),
                    expected_output_revision: None,
                }),
            )
        };

        let shown = super::get_settings(State(state.clone())).await.unwrap();
        assert_eq!(
            shown
                .0
                .settings
                .get("output.naming_profile")
                .map(String::as_str),
            Some("classic")
        );
        assert_eq!(
            shown.0.effective.get("output.revision").map(String::as_str),
            Some("1")
        );

        let ok = patch(vec![
            ("output.format", "single_mp3"),
            ("output.multi_destination", "refetch_all"),
        ])
        .await
        .expect("output-only patch");
        assert_eq!(
            ok.0.settings.get("output.format").map(String::as_str),
            Some("single_mp3")
        );
        let loaded = load_output(&store).await.unwrap();
        assert_eq!(loaded.revision, session.output.revision + 1);
        assert_eq!(
            loaded.body.format,
            bookclerk_config::OutputFormat::SingleMp3
        );
        {
            let live = state.config.read().await;
            assert_eq!(
                live.output.format,
                bookclerk_config::OutputFormat::SingleMp3
            );
            assert_eq!(
                live.output.multi_destination,
                bookclerk_config::MultiDestinationMode::RefetchAll
            );
            assert_eq!(live.output_revision, Some(loaded.revision));
        }
        let audits = audit_count(&store, OUTPUT_NAMESPACE).await.unwrap();

        // A client that read an older revision gets 409 and writes nothing.
        let stale = super::patch_settings(
            State(state.clone()),
            Json(super::PatchSettingsRequest {
                settings: vec![super::SettingsUpdate {
                    key: "output.create_cue".into(),
                    value: "true".into(),
                }],
                expected_output_revision: Some(session.output.revision),
            }),
        )
        .await
        .expect_err("stale expected revision");
        assert_eq!(stale.status(), StatusCode::CONFLICT);
        assert_eq!(load_output(&store).await.unwrap(), loaded);
        assert_eq!(audit_count(&store, OUTPUT_NAMESPACE).await.unwrap(), audits);

        let mixed = patch(vec![
            ("output.create_cue", "true"),
            ("library.auto_acquire", "true"),
        ])
        .await
        .expect_err("mixed authorities");
        assert_eq!(mixed.status(), StatusCode::BAD_REQUEST);
        let invalid = patch(vec![("output.cover_size", "huge")])
            .await
            .expect_err("invalid value");
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        assert_eq!(load_output(&store).await.unwrap(), loaded);
        assert_eq!(audit_count(&store, OUTPUT_NAMESPACE).await.unwrap(), audits);
    }

    #[tokio::test]
    async fn output_settings_revision_tracks_the_document_not_the_applied_runtime() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        use axum::extract::State;
        use axum::Json;
        use bookclerk_library::control_plane::{
            bootstrap_control_plane, load_output, overlay_core_documents, replace_output,
            ConfigActor, OutputReplace,
        };
        use bookclerk_library::LibraryStore;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[daemon]\nlisten = \"127.0.0.1:8787\"\n\n[daemon.auth]\nenabled = true\n",
        )
        .unwrap();
        let mut cfg = bookclerk_config::Config::load(Some(dir.path().to_path_buf()), None).unwrap();
        let db = bookclerk_plugin_database_sqlite::open(&dir.path().join("library.db"))
            .await
            .unwrap();
        bookclerk_library::apply_host_schema(&db).await.unwrap();
        let store = LibraryStore::from_connection(db);
        let scratch = dir.path().join("cache");
        let session = bootstrap_control_plane(&store, dir.path(), &scratch, None, &cfg)
            .await
            .unwrap();
        overlay_core_documents(&mut cfg, &session);
        let state = std::sync::Arc::new(crate::config_authority::control_plane_test_state(
            store.clone(),
            cfg,
        ));

        // Another host commits a new revision; this host has not applied it.
        let mut body = session.output.body.clone();
        body.set("output.create_cue", "true").unwrap();
        let written = match replace_output(
            &store,
            &ConfigActor::Operator { id: "peer".into() },
            session.output.revision,
            &body,
            "peer-write",
        )
        .await
        .unwrap()
        {
            OutputReplace::Applied(doc) => doc,
            other => panic!("unexpected outcome: {other:?}"),
        };

        let shown = super::get_settings(State(state.clone())).await.unwrap().0;
        assert_eq!(
            shown.settings.get("output.create_cue").map(String::as_str),
            Some("true")
        );
        assert_eq!(shown.output_revision, Some(written.revision));
        assert_eq!(
            shown.effective.get("output.revision").map(String::as_str),
            Some(session.output.revision.to_string()).as_deref()
        );

        // The document revision the panel echoes back is accepted.
        let saved = super::patch_settings(
            State(state.clone()),
            Json(super::PatchSettingsRequest {
                settings: vec![super::SettingsUpdate {
                    key: "output.download_pdf".into(),
                    value: (!written.body.download_pdf).to_string(),
                }],
                expected_output_revision: shown.output_revision,
            }),
        )
        .await
        .expect("document revision is current")
        .0;
        let loaded = load_output(&store).await.unwrap();
        assert_eq!(loaded.revision, written.revision + 1);
        assert_eq!(saved.output_revision, Some(loaded.revision));
        assert_ne!(loaded.body.download_pdf, written.body.download_pdf);
        assert!(loaded.body.create_cue);
    }

    #[tokio::test]
    async fn reload_events_read_failure_keeps_the_previous_runtime() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        use async_trait::async_trait;
        use bookclerk_integrations::{
            DomainEvent, EventResult, Integration, IntegrationContext, IntegrationHealth,
            IntegrationRegistry,
        };
        use bookclerk_library::control_plane::{
            bootstrap_control_plane, load_events, overlay_events,
        };
        use bookclerk_library::LibraryStore;

        struct StopProbe {
            stopped: Arc<AtomicBool>,
        }

        #[async_trait]
        impl Integration for StopProbe {
            fn id(&self) -> &str {
                "stop-probe"
            }

            async fn start(&self, _ctx: IntegrationContext) -> bookclerk_integrations::Result<()> {
                Ok(())
            }

            async fn stop(&self) -> bookclerk_integrations::Result<()> {
                self.stopped.store(true, Ordering::SeqCst);
                Ok(())
            }

            async fn health(&self) -> bookclerk_integrations::Result<IntegrationHealth> {
                Ok(IntegrationHealth {
                    id: "stop-probe".into(),
                    enabled: true,
                    ok: true,
                    detail: None,
                })
            }

            async fn deliver_domain_event(
                &self,
                _event: DomainEvent,
            ) -> bookclerk_integrations::Result<EventResult> {
                Ok(EventResult::Ack)
            }
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[daemon]\nlisten = \"127.0.0.1:8787\"\n\n[daemon.auth]\nenabled = false\n",
        )
        .unwrap();
        let mut cfg = bookclerk_config::Config::load(Some(dir.path().to_path_buf()), None).unwrap();
        let db = bookclerk_plugin_database_sqlite::open(&dir.path().join("library.db"))
            .await
            .unwrap();
        bookclerk_library::apply_host_schema(&db).await.unwrap();
        let store = LibraryStore::from_connection(db);
        let scratch = dir.path().join("cache");
        let session = bootstrap_control_plane(&store, dir.path(), &scratch, None, &cfg)
            .await
            .unwrap();
        overlay_events(&mut cfg, &session.events, &session.cluster_id);
        let revision = session.events.revision;
        let retention = session.events.body.retention_days;
        let stopped = Arc::new(AtomicBool::new(false));
        let mut app = crate::config_authority::control_plane_test_state(store.clone(), cfg);
        app.integrations = Arc::new(tokio::sync::RwLock::new(IntegrationRegistry::new()));
        app.integrations.write().await.register(Arc::new(StopProbe {
            stopped: Arc::clone(&stopped),
        }));
        let state = Arc::new(app);
        let auth_before = state.auth.read().await.token.clone();
        let err = crate::config_authority::FAIL_EVENTS_PUBLICATION_READ
            .scope((), super::reload_daemon_config_held(&state))
            .await
            .expect_err("injected read must fail the reload");
        assert!(
            err.to_string()
                .contains("injected events publication read failure"),
            "{err}"
        );
        assert!(
            !stopped.load(Ordering::SeqCst),
            "old integration was stopped"
        );
        assert_eq!(state.integrations.read().await.all().len(), 1);
        assert_eq!(state.integrations.read().await.all()[0].id(), "stop-probe");
        assert_eq!(state.auth.read().await.token, auth_before);
        let live = state.config.read().await;
        assert_eq!(live.events_revision, Some(revision));
        assert_eq!(live.events.retention_days, retention);
        assert_eq!(
            live.events_authority.as_deref(),
            Some(session.cluster_id.as_str())
        );
        let loaded = load_events(&store).await.unwrap();
        assert_eq!(loaded.revision, revision);
        assert_eq!(loaded.body.retention_days, retention);
    }

    /// A live invite serves the app's document under its policy, and a
    /// missing build is a 404.
    #[tokio::test]
    async fn the_invite_page_serves_the_app_under_its_policy() {
        let dist = tempfile::tempdir().unwrap();
        let index = dist.path().join("index.html");
        std::fs::write(&index, "<!doctype html><title>SPA</title>").unwrap();
        let res = super::serve_invite_spa(&index).await;
        assert_eq!(res.status(), axum::http::StatusCode::OK);
        assert_eq!(
            res.headers()[axum::http::header::CONTENT_SECURITY_POLICY],
            super::SPA_CSP
        );
        let missing = super::serve_invite_spa(&dist.path().join("gone.html")).await;
        assert_eq!(missing.status(), axum::http::StatusCode::NOT_FOUND);
    }

    /// Library pages filter, sort, and paginate in SQL, including the search path.
    #[tokio::test]
    async fn library_books_page_filters_in_sql_and_limits_search_hydration() {
        use std::sync::Arc;

        use axum::extract::{Query, State};
        use axum::http::HeaderMap;
        use bookclerk_integrations::IntegrationRegistry;
        use bookclerk_library::{AcquireStatus, LibraryStore, NewBook};
        use bookclerk_plugin_host::{DatabaseRegistry, DestinationRegistry};
        use bookclerk_search::SearchEngine;
        use bookclerk_source::SourceRegistry;
        use tokio::sync::{Mutex, Notify, RwLock, Semaphore};

        use crate::api::AppState;
        use crate::auth::OperatorAuthState;

        let files = tempfile::tempdir().unwrap();
        let mut cfg = Config::load(Some(files.path().to_path_buf()), None).unwrap();
        cfg.daemon.auth.enabled = false;
        let store = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        );
        store
            .upsert_account("audible", "envelope-a", "us", None, false, "audible")
            .await
            .unwrap();
        store
            .upsert_account("audible", "envelope-b", "us", None, false, "audible")
            .await
            .unwrap();
        for i in 0..20u32 {
            let account = if i < 16 { "envelope-a" } else { "envelope-b" };
            let book = NewBook::minimal(format!("B{i:05}"), account, "us", format!("Title {i:05}"));
            let saved = store.upsert_book(&book).await.unwrap();
            let status = if i % 10 == 0 {
                AcquireStatus::Error
            } else {
                AcquireStatus::Acquired
            };
            store
                .set_acquire_status(&saved.uuid, &saved.account_id, status, None, None)
                .await
                .unwrap();
            if i == 0 {
                store
                    .update_catalog_enrichment(
                        &saved.uuid,
                        &bookclerk_library::CatalogEnrichmentFields {
                            cover_url: Some("https://img.example/c.jpg".into()),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
            }
        }
        SearchEngine::open(&cfg.paths().search_index_dir)
            .unwrap()
            .rebuild(&store)
            .await
            .unwrap();

        let state = Arc::new(AppState {
            config: Arc::new(RwLock::new(cfg)),
            library: Arc::new(RwLock::new(store)),
            database_registry: Arc::new(RwLock::new(DatabaseRegistry::default())),
            job_notify: Arc::new(Notify::new()),
            job_runtime: Arc::new(RwLock::new(())),
            work_lock: Mutex::new(()),
            discover_gate: Arc::new(Semaphore::new(1)),
            integrations: Arc::new(RwLock::new(IntegrationRegistry::new())),
            sources: Arc::new(RwLock::new(SourceRegistry::new())),
            destinations: Arc::new(RwLock::new(DestinationRegistry::default())),
            auth: Arc::new(RwLock::new(Arc::new(OperatorAuthState::new(
                String::new(),
                12,
                false,
                5,
                30,
            )))),
            reload_lock: Mutex::new(()),
            listen_reload: Arc::new(Notify::new()),
            last_bound_listen: RwLock::new(None),
            tray: RwLock::new(None),
            tray_handoff: Mutex::new(None),
            deployment_runtime: std::sync::OnceLock::new(),
            node_api_tls_bound: std::sync::OnceLock::new(),
            library_swaps: tokio::sync::watch::Sender::new(0),
            covers: Arc::new(crate::covers::CoverService::detached()),
        });

        let page = super::list_books(
            State(state.clone()),
            HeaderMap::new(),
            Query(super::BooksQuery {
                account: None,
                status: None,
                q: None,
                limit: Some(5),
                offset: Some(0),
            }),
        )
        .await
        .unwrap();
        assert_eq!(page.total, 20);
        assert_eq!(page.books.len(), 5);
        assert_eq!(page.limit, 5);
        assert_eq!(page.books[0].title, "Title 00000");
        assert!(page.books[0].title < page.books[4].title);
        // A remote cover gets a same-origin path; a title without one doesn't.
        let first = &page.books[0];
        let path = first.cover_path.as_deref().expect("cover path");
        assert!(path.starts_with("/api/covers/"), "{path}");
        assert_eq!(
            state
                .covers
                .open_token(&path["/api/covers/".len()..])
                .as_deref(),
            Some("https://img.example/c.jpg")
        );
        assert_eq!(page.books[1].cover_path, None);
        let one = super::get_book(
            State(state.clone()),
            axum::extract::Path(first.uuid.clone()),
        )
        .await
        .unwrap();
        assert_eq!(one.cover_path.as_deref(), Some(path));
        assert_eq!(
            super::get_book(State(state.clone()), axum::extract::Path("missing".into()))
                .await
                .map(|_| ())
                .unwrap_err(),
            axum::http::StatusCode::NOT_FOUND
        );
        // Gravatars and store icons come through the same service: a hash
        // that is not a SHA-256 or a store nobody deployed is a 404, and a
        // node with no cover cache answers 503.
        let status = |res: axum::response::Response| res.status();
        let gravatar = |hash: &str| {
            crate::covers::get_gravatar(
                State(state.clone()),
                axum::extract::Path(hash.to_owned()),
                HeaderMap::new(),
            )
        };
        assert_eq!(
            status(
                crate::covers::get_image(
                    State(state.clone()),
                    axum::extract::Path("forged.token".into()),
                    HeaderMap::new()
                )
                .await
            ),
            axum::http::StatusCode::NOT_FOUND
        );
        assert_eq!(
            status(gravatar("not-a-hash").await),
            axum::http::StatusCode::NOT_FOUND
        );
        assert_eq!(
            status(gravatar(&"a".repeat(64)).await),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            status(
                crate::covers::get_store_icon(
                    State(state.clone()),
                    axum::extract::Path("nowhere".into()),
                    HeaderMap::new()
                )
                .await
            ),
            axum::http::StatusCode::NOT_FOUND
        );

        let acquired = super::list_books(
            State(state.clone()),
            HeaderMap::new(),
            Query(super::BooksQuery {
                account: None,
                status: Some("acquired".into()),
                q: None,
                limit: Some(40),
                offset: Some(0),
            }),
        )
        .await
        .unwrap();
        assert_eq!(acquired.total, 18);
        assert_eq!(acquired.books.len(), 18);
        assert!(acquired
            .books
            .iter()
            .all(|book| book.acquire_status == AcquireStatus::Acquired));

        let searched = super::list_books(
            State(state.clone()),
            HeaderMap::new(),
            Query(super::BooksQuery {
                account: None,
                status: None,
                q: Some("Title".into()),
                limit: Some(8),
                offset: Some(0),
            }),
        )
        .await
        .unwrap();
        assert_eq!(searched.books.len(), 8);
        assert_eq!(searched.total, 20, "every hit counts, not only one page");
        assert_eq!(searched.books[0].title, "Title 00000");
        // The index pages in title order, so the next page continues it.
        let next = super::list_books(
            State(state.clone()),
            HeaderMap::new(),
            Query(super::BooksQuery {
                account: None,
                status: None,
                q: Some("Title".into()),
                limit: Some(8),
                offset: Some(16),
            }),
        )
        .await
        .unwrap();
        assert_eq!(next.total, 20);
        let titles: Vec<&str> = next.books.iter().map(|b| b.title.as_str()).collect();
        assert_eq!(
            titles,
            ["Title 00016", "Title 00017", "Title 00018", "Title 00019"]
        );
        // A status filter is read from the database, not the index.
        let failed = super::list_books(
            State(state.clone()),
            HeaderMap::new(),
            Query(super::BooksQuery {
                account: None,
                status: Some("error".into()),
                q: Some("Title".into()),
                limit: Some(40),
                offset: Some(0),
            }),
        )
        .await
        .unwrap();
        assert_eq!(failed.total, 2);

        let account_b = super::list_books(
            State(state),
            HeaderMap::new(),
            Query(super::BooksQuery {
                account: Some("envelope-b".into()),
                status: None,
                q: Some("Title".into()),
                limit: Some(40),
                offset: Some(0),
            }),
        )
        .await
        .unwrap();
        assert_eq!(account_b.total, 4);
        assert_eq!(account_b.books.len(), 4);
        assert!(account_b
            .books
            .iter()
            .all(|book| book.account_id == "envelope-b"));
    }

    /// With no local destination on the node, a title's sidecar cover is a
    /// 404 whether or not the title exists, answered without a library read.
    #[tokio::test]
    async fn book_cover_is_not_found_without_a_local_destination() {
        use axum::body::Body;
        use axum::http::{header, Request, StatusCode};
        use tower::ServiceExt;

        let (state, app, _library) = crate::auth::tests::phase2_harness("op-token-cover").await;
        assert!(state.destinations.read().await.local().is_none());
        let cookie = crate::auth::operator_session_cookie(&state).await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/library/books/no-such-title/cover")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
}
