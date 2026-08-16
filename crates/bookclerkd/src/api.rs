//! HTTP control plane for `bookclerkd` (operator API + static GUI).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::Request;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::Json;
use axum::Router;
use bookclerk_acquire::sidecar_key;
use bookclerk_config::{Config, ListenAddrs};
use bookclerk_integrations::{portal_spa_router, IntegrationRegistry, PortalState};
use bookclerk_library::{
    configure_master_key_with, hash_token, AcquireStatus, BookRecord, ClaimTicketRecord, JobRecord,
    JobTrigger, LibraryStore, NewTitleRequest, NewTitleRequestSource, QueueWisher, RequestStatus,
    TitleRequestRecord,
};
use bookclerk_plugin_host::{
    consent_request, consent_summary, cores_to_percent, effective_cpu_cores, format_cpu_cores,
    grant_covers, host_cpu_cores_max, percent_to_cores, require_grant, validate_approved_grant,
    DatabaseRegistry, DestinationRegistry, PluginGrant, PluginGrantStore, PluginRuntimeKind,
    WorkerdLimits, KNOWN_HOST_BINDINGS, PLUGIN_JAIL_CPU_CORES_DEFAULT,
    PLUGIN_JAIL_CPU_RATE_DEFAULT, PLUGIN_JAIL_EXTRA_PROCESSES_DEFAULT,
    PLUGIN_JAIL_EXTRA_PROCESSES_MAX, PLUGIN_JAIL_MEMORY_MIB_DEFAULT, PLUGIN_JAIL_MEMORY_MIB_MAX,
    PLUGIN_STATE_BUDGET_MIB_DEFAULT, PLUGIN_STATE_BUDGET_MIB_MAX,
};
use bookclerk_search::{SearchEngine, SearchHit};
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

use crate::auth::{self, OperatorAuthState};
use crate::http_error;
use crate::jobs::{
    enqueue_acquire, enqueue_integration_scan, enqueue_listen_sync, enqueue_scan, AdmitJob,
};

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
}

#[derive(Clone)]
/// Serves `GET /invite` (validate magic link, then the SPA or a branded 4xx).
struct InvitePageState {
    /// Daemon state for library ticket lookup.
    app: Arc<AppState>,
    /// `ui/dist/index.html` when the GUI is packaged beside the daemon.
    index: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Default)]
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
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            )],
            bytes,
        )
            .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Snapshot of one queued or running control-plane job.
pub struct JobInfo {
    /// Opaque job id returned to the client when the job is accepted.
    pub id: String,
    /// Job type (`scan`, `acquire`, `listen_sync`).
    pub kind: String,
    /// Lifecycle string (`pending`, `running`, `succeeded`, `failed`, `cancelled`).
    pub status: String,
    /// Optional operator-facing progress or error text.
    pub detail: Option<String>,
    /// Human-readable progress string while the job is running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<String>,
    /// Number of times a worker has claimed this row.
    #[serde(default)]
    pub attempt_count: i64,
    /// Maximum claims before a failure is terminal.
    #[serde(default)]
    pub max_attempts: i64,
    /// Structured error kind when failed or cancelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
    /// RFC 3339 timestamp when the row was inserted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// RFC 3339 timestamp when the row reached a terminal state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
}

#[derive(Debug, Serialize)]
/// Liveness payload for unauthenticated `GET /health`.
struct HealthResponse {
    /// Always `ok` when the process can serve HTTP.
    status: &'static str,
}

#[derive(Debug, Serialize)]
/// Library and daemon snapshot for `GET /api/status`.
struct StatusResponse {
    /// Count of configured store accounts.
    accounts: usize,
    /// Total title rows in the library.
    books: usize,
    /// Titles whose `acquire_status` is `acquired`.
    acquired: i64,
    /// Titles still needing acquire (`not_acquired`).
    pending: i64,
    /// Titles stuck in `error` after a failed acquire.
    error: i64,
    /// Titles currently `queued` or `downloading`.
    in_progress: i64,
    /// Comma-joined `daemon.listen` addresses from live config.
    listen: String,
    /// Comma-joined enabled destination plugin ids, or `none`.
    storage_backend: String,
    /// Whether the live auth middleware requires operator credentials.
    auth_enabled: bool,
}

#[derive(Debug, Deserialize)]
/// One `config.toml` key/value pair from a settings PATCH.
struct SettingsUpdate {
    /// Dotted config key; rejected unless it passes the settings allowlist.
    key: String,
    /// Raw string value, normalized before it is written.
    value: String,
}

#[derive(Debug, Deserialize)]
/// Body for `PATCH /api/settings`.
struct PatchSettingsRequest {
    /// Key/value pairs to validate, persist, and reload.
    settings: Vec<SettingsUpdate>,
}

#[derive(Debug, Serialize)]
/// One selectable value for a Settings dropdown.
struct PluginSettingChoice {
    /// Stored config value; empty string means use the plugin default.
    value: String,
    /// Operator-facing choice caption.
    label: String,
}

#[derive(Debug, Serialize)]
/// One editable setting shown in the Settings UI.
struct PluginSettingOption {
    /// Dotted `config.toml` key for this option.
    key: String,
    /// Operator-facing field caption.
    label: String,
    /// Current string form of the setting.
    value: String,
    /// UI widget hint (`boolean`, `string`, or `number`).
    value_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional dropdown values; omitted for free-form fields.
    choices: Option<Vec<PluginSettingChoice>>,
}

#[derive(Debug, Serialize)]
/// Settings UI group for one discovered or loaded plugin.
struct PluginSettingsGroup {
    /// Plugin id (`audible`, `sqlite`, and similar).
    id: String,
    /// Plugin kind wire label (`source`, `integration`, `output`, `database`).
    kind: String,
    /// Google favicon (or portal brand) URL for Settings list rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    logo: Option<String>,
    /// Editable options for this plugin, including `.enabled`.
    settings: Vec<PluginSettingOption>,
}

#[derive(Debug, Serialize)]
/// Settings page payload: file values, effective runtime, and plugin groups.
struct SettingsResponse {
    /// Values as written in config.toml (and env overlays).
    settings: std::collections::BTreeMap<String, String>,
    /// Runtime-effective values after the last successful reload (auth, plugins).
    effective: std::collections::BTreeMap<String, String>,
    /// Per-plugin setting groups rendered by the Settings UI.
    plugins: Vec<PluginSettingsGroup>,
    /// Host max jail CPU in cores (2 d.p.; equals logical CPU count).
    host_cpu_cores_max: f64,
    /// Optional global per-jail CPU ceiling in cores (from `[plugins.jail]`).
    #[serde(skip_serializing_if = "Option::is_none")]
    jail_cpu_cores: Option<f64>,
}

#[derive(Debug, Serialize)]
/// Optional branding shown on the plugin consent dialog.
struct PluginConsentBrand {
    /// Display name from the portal brand or plugin manifest.
    name: String,
    /// Background CSS color from the portal brand, when known.
    bg: Option<String>,
    /// Foreground CSS color from the portal brand, when known.
    fg: Option<String>,
    /// Accent CSS color from the portal brand, when known.
    accent: Option<String>,
    /// Logo URL or host-served embed path.
    logo: Option<String>,
}

#[derive(Debug, Serialize)]
/// Host-capped resource defaults shown on the consent dialog.
struct PluginConsentLimits {
    /// Requested workerd CPU budget in milliseconds.
    cpu_ms: u32,
    /// Requested workerd subrequest budget.
    subrequests: u32,
    /// Host hard cap for workerd CPU milliseconds.
    max_cpu_ms: u32,
    /// Host hard cap for workerd subrequests.
    max_subrequests: u32,
    /// Default plugin state disk budget in mebibytes.
    disk_mib: u32,
    /// Host hard cap for plugin state disk in mebibytes.
    max_disk_mib: u32,
    /// Default jail memory in mebibytes.
    memory_mib: u32,
    /// Host hard cap for jail memory in mebibytes.
    max_memory_mib: u32,
    /// Manifest / default jail CPU in cores (2 d.p.).
    cpu_cores: f64,
    /// Host max jail CPU in cores (2 d.p.).
    max_cpu_cores: f64,
    /// Optional `[plugins.jail]` ceiling in cores.
    #[serde(skip_serializing_if = "Option::is_none")]
    jail_cpu_cores: Option<f64>,
    /// Default extra process/thread budget (native consent).
    extra_processes: u32,
    /// Host hard cap for extra process budget.
    max_extra_processes: u32,
    /// Suggested / default host bindings operators may grant.
    known_bindings: Vec<&'static str>,
}

/// Operator-facing grant JSON: CPU as cores (not Spec percent).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PluginGrantView {
    /// Plugin id this grant applies to.
    plugin_id: String,
    /// Plugin kind string copied from the stored grant.
    kind: String,
    /// Approved egress mode (`deny`, `allowlist`, and similar).
    network_mode: String,
    /// Hostnames the operator approved for egress.
    domains: Vec<String>,
    /// Host bindings the operator approved.
    bindings: Vec<String>,
    /// Extra compatibility flags stored on the grant.
    compatibility_flags: Vec<String>,
    /// RFC 3339 timestamp when the grant was last approved.
    approved_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional workerd CPU budget in milliseconds.
    cpu_ms: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional workerd subrequest budget.
    subrequests: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional plugin state disk budget in mebibytes.
    disk_mib: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional jail memory in mebibytes.
    memory_mib: Option<u32>,
    /// Jail CPU in cores (2 d.p.); absent for workerd.
    #[serde(skip_serializing_if = "Option::is_none")]
    cpu_cores: Option<f64>,
    /// Extra process/thread budget beyond launcher overhead; native only.
    #[serde(skip_serializing_if = "Option::is_none")]
    extra_processes: Option<u32>,
}

impl PluginGrantView {
    /// Projects a stored grant into operator-facing camelCase JSON, converting CPU percent to cores.
    fn from_grant(grant: &PluginGrant) -> Self {
        Self {
            plugin_id: grant.plugin_id.clone(),
            kind: grant.kind.clone(),
            network_mode: grant.network_mode.clone(),
            domains: grant.domains.iter().cloned().collect(),
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

#[derive(Debug, Serialize)]
/// Consent dialog payload for one plugin.
struct PluginConsentResponse {
    /// Plugin id whose grant is being reviewed.
    plugin_id: String,
    /// Guest runtime (`native` or `workerd`) — drives which controls are enforceable.
    runtime: String,
    /// Manifest-derived grant the plugin is asking for.
    request: PluginGrantView,
    /// True when an existing on-disk grant already satisfies the request.
    covered: bool,
    /// Operator-facing bullet list of requested capabilities.
    summary: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Previously approved grant, when one is stored.
    existing: Option<PluginGrantView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional portal or manifest branding for the dialog.
    brand: Option<PluginConsentBrand>,
    /// Host-capped resource defaults (workerd budgets + shared disk). Always set.
    limits: PluginConsentLimits,
}

#[derive(Debug, Deserialize)]
/// Body for `POST /api/plugins/{id}/consent`.
struct PluginConsentApproveRequest {
    /// Must be true; a false value is rejected with 400.
    approve: bool,
    #[serde(default)]
    /// Optional operator overrides applied on top of the manifest request.
    grant: Option<PluginGrantOverride>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Operator-supplied grant fields that replace matching request values.
struct PluginGrantOverride {
    /// Override egress mode when set.
    network_mode: Option<String>,
    /// Override approved hostnames when set.
    domains: Option<Vec<String>>,
    /// Override approved host bindings when set.
    bindings: Option<Vec<String>>,
    /// Override compatibility flags when set.
    compatibility_flags: Option<Vec<String>>,
    /// Override workerd CPU milliseconds when set.
    cpu_ms: Option<u32>,
    /// Override workerd subrequest budget when set.
    subrequests: Option<u32>,
    /// Override disk budget in mebibytes when set.
    disk_mib: Option<u32>,
    /// Override jail memory in mebibytes when set.
    memory_mib: Option<u32>,
    /// Jail CPU in cores (2 d.p.); converted to Spec percent server-side.
    cpu_cores: Option<f64>,
    /// Extra process/thread budget (native); ignored for workerd.
    extra_processes: Option<u32>,
}

#[derive(Debug, Serialize)]
/// Generic mutation result with success flag, message, and optional job id.
struct ActionResponse {
    /// True when the action was accepted or completed.
    ok: bool,
    /// Operator-facing status text.
    message: String,
    /// Queued job id, or empty when the action is synchronous.
    job_id: String,
    /// Machine slug when admission failed (`conflict`, `queue_full`).
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
/// Optional body for `POST /api/library/scan`.
pub struct ScanRequest {
    /// When set, scan this account even if `scan_enabled` is false.
    pub account: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
/// Optional body for `POST /api/library/acquire`.
pub struct AcquireRequestBody {
    /// Audible ASIN filter; first of uuid/asin/isbn/product_id wins.
    pub asin: Option<String>,
    /// Library title UUID filter.
    pub uuid: Option<String>,
    /// ISBN filter for non-Audible stores.
    pub isbn: Option<String>,
    /// Store product id filter.
    pub product_id: Option<String>,
    /// Restrict acquire to this store account.
    pub account: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
/// Optional body for `POST /integrations/{id}/scan`.
pub struct IntegrationScanRequest {
    /// When true, the integration rescans even if it considers itself current.
    pub force: Option<bool>,
}

#[derive(Debug, Deserialize)]
/// Body for copying library rows between database plugins.
struct DatabaseMigrateRequest {
    /// Source plugin id. Defaults to the active `[database].plugin` before reload.
    from: Option<String>,
    /// Destination database plugin id to copy into.
    to: String,
    #[serde(default)]
    /// When true, count rows without writing.
    dry_run: bool,
    #[serde(default)]
    /// When true, overwrite a destination that is not empty.
    force: bool,
    /// Update `[database].plugin` in config.toml after a successful copy.
    #[serde(default)]
    apply: bool,
}

#[derive(Debug, Deserialize, Default)]
/// Query string for `GET /api/library/books`.
struct BooksQuery {
    /// Restrict results to this store account id.
    account: Option<String>,
    /// Optional `AcquireStatus` wire-string filter.
    status: Option<String>,
    /// Full-text search query; empty skips the search index.
    q: Option<String>,
    /// Page size, clamped to 1–500 (default 40).
    limit: Option<usize>,
    /// Number of matching titles to skip.
    offset: Option<usize>,
}

#[derive(Debug, Serialize)]
/// Paginated library listing.
struct BooksResponse {
    /// Current page of title records.
    books: Vec<BookRecord>,
    /// Match count before pagination.
    total: usize,
    /// Applied page size.
    limit: usize,
    /// Applied skip count.
    offset: usize,
}

/// Build the HTTP router (API + optional static UI + SPA portal APIs).
pub fn router(state: Arc<AppState>, ui_dist: Option<PathBuf>) -> Router {
    let portal_state = PortalState {
        config: state.config.clone(),
        library: state.library.clone(),
        integrations: state.integrations.clone(),
        sources: state.sources.clone(),
    };

    // Use `route_layer` (not `layer`) so auth never wraps the router's fallback.
    // `.layer(auth)` made unmatched paths — including `/` when the GUI dist was
    // missing — return a bare 401 instead of serving the SPA / branded 404.
    let operator_or_admin = Router::new()
        .route("/status", get(status))
        .route("/scan", post(trigger_scan))
        .route("/acquire", post(trigger_acquire))
        .route("/jobs", get(list_jobs))
        .route("/jobs/{id}/cancel", post(cancel_job))
        .route("/integrations/{id}/scan", post(trigger_integration_scan))
        .route("/api/status", get(status))
        .route("/api/jobs", get(list_jobs))
        .route("/api/jobs/{id}/cancel", post(cancel_job))
        .route("/api/library/scan", post(trigger_scan))
        .route("/api/library/acquire", post(trigger_acquire))
        .route("/api/discover/sync-listening", post(sync_listening))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_operator_or_administrator_auth,
        ))
        .with_state(state.clone());

    let operator_only = Router::new()
        .route("/api/config/reload", post(reload_config))
        .route("/api/settings", get(get_settings).patch(patch_settings))
        .route(
            "/api/plugins/{id}/consent",
            get(get_plugin_consent).post(post_plugin_consent),
        )
        .route("/api/plugins/{kind}/{id}/logo", get(get_plugin_logo))
        .route("/api/database/migrate", post(migrate_database))
        .route(
            "/api/auth/impersonate",
            post(auth::impersonate).delete(auth::impersonate_end),
        )
        .route("/api/auth/elevate", delete(auth::elevate_end))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_operator_auth,
        ))
        .with_state(state.clone());

    let shared = Router::new()
        .route("/api/library/books", get(list_books))
        .route("/api/library/books/{uuid}", get(get_book))
        .route("/api/library/books/{uuid}/cover", get(get_book_cover))
        .route(
            "/api/discover/recommendations",
            get(discover_recommendations),
        )
        .route(
            "/api/discover/purchase-hints",
            post(discover_purchase_hints),
        )
        .route(
            "/api/discover/purchase-hints/batch",
            post(discover_purchase_hints_batch),
        )
        .route("/api/discover/search", get(discover_catalog_search))
        .route("/api/discover/title-meta", post(discover_title_meta))
        .route(
            "/api/discover/title-meta/batch",
            post(discover_title_meta_batch),
        )
        .route("/api/discover/title-reviews", post(discover_title_reviews))
        .route("/api/wishlist", get(list_wishlist).post(create_wishlist))
        .route("/api/wishlist/{uuid}", delete(delete_wishlist))
        .route("/api/request-queue", get(list_request_queue))
        .route("/api/auth/logout", post(auth::logout))
        .route("/api/auth/sessions", get(auth::list_sessions))
        .route("/api/auth/sessions/{id}", delete(auth::revoke_session))
        .route("/api/auth/profile", patch(crate::profile::patch_profile))
        .route(
            "/api/auth/profile/avatar",
            put(crate::profile::put_avatar).delete(crate::profile::delete_avatar),
        )
        .route("/api/users/{id}/avatar", get(crate::profile::get_avatar))
        .route(
            "/api/preferences",
            get(get_preferences).patch(patch_preferences),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_operator_or_portal_auth,
        ))
        .with_state(state.clone());

    // `/api/auth/me` stays public so the SPA bootstrap can probe session state
    // without a bare middleware 401 (which some browsers render as an error
    // document). The handler itself reports operator / portal / anonymous.
    let mut app = Router::new()
        .route("/health", get(health))
        .route("/api/auth/login", post(auth::login))
        .route("/api/auth/signin", get(auth::signin_methods))
        .route(
            "/api/auth/tray-handoff/prepare",
            post(auth::tray_handoff_prepare),
        )
        .route("/api/auth/tray-handoff", get(auth::tray_handoff))
        .route("/api/auth/me", get(auth::me))
        .route("/api/auth/elevate", post(auth::elevate))
        .route("/api/auth/bootstrap", post(auth::bootstrap))
        .route(
            "/api/auth/password",
            post(auth::password_login).put(auth::set_password),
        )
        .route("/api/users", get(auth::list_users).post(auth::create_user))
        .route(
            "/api/users/{id}",
            axum::routing::patch(auth::patch_user).delete(auth::delete_user),
        )
        .route(
            "/api/users/{id}/claim-ticket",
            post(auth::create_user_claim_ticket),
        )
        .route(
            "/api/users/{id}/reset-password",
            post(auth::reset_user_password),
        )
        .merge(operator_or_admin)
        .merge(operator_only)
        .merge(shared)
        .with_state(state.clone());

    // SPA Accounts / claim APIs (Path=/ portal session cookie).
    app = app.nest("/api/portal", portal_spa_router(portal_state));

    // OIDC authorization server (ABS / third-party user tokens).
    app = app.merge(crate::oidc::router(state.clone()));
    // Optional upstream OIDC/OAuth relying party (identity broker).
    app = app.merge(crate::oidc_rp::router(state.clone()));
    // WebAuthn passkeys (login + Owner elevate).
    app = app.merge(crate::passkeys::router(state.clone()));
    // TOTP MFA + host second-factor policy.
    app = app.merge(crate::totp::router(state.clone()));

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
                "/settings",
            ];
            for path in SPA_DOC_PATHS {
                app = app.route_service(path, ServeFile::new(index.clone()));
            }
            app = app.fallback_service(ServeDir::new(dist));
        } else {
            tracing::warn!(
                path = %dist.display(),
                "GUI dist path missing; static UI not served (build ui/ or set BOOKCLERK_UI_DIST)"
            );
        }
    }

    // HTML documents get Cache-Control: no-cache (`/invite` is no-store).
    // Brand empty 4xx/5xx bodies (auth, missing routes, handler StatusCode).
    // Timeout wraps API handlers; normalize trailing slashes before matching.
    // Trace outermost.
    app.layer(middleware::from_fn(html_document_no_cache))
        .layer(middleware::from_fn(api_timeout_middleware))
        .layer(middleware::from_fn(http_error::brand_error_responses))
        .layer(NormalizePathLayer::trim_trailing_slash())
        .layer(
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
                            tracing::info!(
                                status = status.as_u16(),
                                latency_ms,
                                "request completed"
                            );
                        }
                    },
                )
                .on_failure(
                    |failure: ServerErrorsFailureClass,
                     latency: Duration,
                     _span: &tracing::Span| {
                        tracing::error!(
                            failure = ?failure,
                            latency_ms = latency.as_millis() as u64,
                            "request failed"
                        );
                    },
                ),
        )
}

/// Opens the discover embedder on a blocking thread so ONNX load cannot stall the HTTP runtime.
async fn open_embedder_blocking(
    models_dir: PathBuf,
    embed_intra_threads: usize,
    embeddings_enabled: bool,
) -> Result<Box<dyn bookclerk_discover::Embedder>, (StatusCode, String)> {
    tokio::task::spawn_blocking(move || {
        bookclerk_discover::open_embedder(&models_dir, embed_intra_threads, embeddings_enabled)
    })
    .await
    .map_err(|err| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("embedder task failed: {err}"),
        )
    })?
    .map_err(internal_err)
}

/// Sets HTML cache headers. `/invite` is `no-store` because the same URL can
/// change from a live SPA to a 410 after redeem; other documents use `no-cache`
/// so a rebuilt `ui/dist/index.html` is used on the next navigation. Hashed
/// JS/CSS under `/assets/` stay cacheable.
async fn html_document_no_cache(req: Request, next: Next) -> Response {
    let invite = req.uri().path() == "/invite";
    let mut res = next.run(req).await;
    let is_html = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/html"));
    if is_html {
        res.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(if invite {
                "no-store, no-cache, must-revalidate"
            } else {
                "no-cache"
            }),
        );
        if invite {
            res.headers_mut()
                .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
        }
    }
    res
}

/// Enforces per-path API deadlines and returns 504 when a `/api/` handler overruns its budget.
async fn api_timeout_middleware(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let method = req.method().to_string();
    // Keep `/health` free of the API timeout so liveness probes stay cheap.
    if path == "/health" || !path.starts_with("/api/") {
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
            (StatusCode::GATEWAY_TIMEOUT, "request timed out").into_response()
        }
    }
}

/// Selects the request deadline in seconds for a control-plane path (8s default; longer for discover).
fn api_timeout_for_path(path: &str) -> Duration {
    match path {
        "/api/discover/purchase-hints"
        | "/api/discover/purchase-hints/batch"
        | "/api/discover/title-meta"
        | "/api/discover/title-meta/batch"
        | "/api/discover/title-reviews" => Duration::from_secs(25),
        "/api/discover/recommendations" => Duration::from_secs(20),
        // Parallel store searches (~7s each) + optional page enrich (~3.5s).
        "/api/discover/search" => Duration::from_secs(16),
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
async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

/// Re-open the library connection for the active `[database].plugin` and refresh destinations.
///
/// Prefer [`reload_daemon_config`] / [`reload_daemon_config_held`] for HTTP
/// mutation routes so config publication stays under `reload_lock`.
#[allow(dead_code)]
pub async fn reload_library_store(state: &AppState, config: &Config) -> anyhow::Result<()> {
    let _runtime = timeout(Duration::from_secs(60), state.job_runtime.write())
        .await
        .map_err(|_| {
            anyhow::anyhow!("timed out waiting for in-flight jobs before database swap")
        })?;
    let registry = bookclerk_plugin_host::load_external_database(config).await?;
    let library = bookclerk_plugin_host::open_library_store(config, &registry).await?;
    let destinations =
        bookclerk_plugin_host::load_external_destinations(config, Some(library.db())).await?;
    *state.database_registry.write().await = registry;
    *state.library.write().await = library;
    *state.destinations.write().await = destinations;
    tracing::info!(
        plugin = %config.database.plugin,
        "reloaded library database connection"
    );
    Ok(())
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
    if let Err(err) = registry.start_all(ctx).await {
        tracing::warn!(%err, "integration start_all reported errors");
    }
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
pub(crate) async fn reload_daemon_config_held(state: &AppState) -> anyhow::Result<String> {
    let (files_dir, config_path, old_listen, old_db_plugin, old_auth_enabled, old_token) = {
        let cfg = state.config.read().await;
        let auth = state.auth.read().await;
        (
            cfg.paths().files_dir.clone(),
            cfg.paths().config_file.clone(),
            cfg.daemon.listen.clone(),
            cfg.database.plugin.clone(),
            auth.enabled,
            auth.token.clone(),
        )
    };

    let new_cfg = Config::load(Some(files_dir.clone()), Some(config_path.clone()))?;
    validate_daemon_listen(&new_cfg)?;
    configure_master_key_with(&files_dir, new_cfg.auth_password().as_deref())?;
    new_cfg.warn_unsupported_options();

    // Build the full candidate before mutating live state.
    let db_plugin_changed = !old_db_plugin.eq_ignore_ascii_case(&new_cfg.database.plugin);

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

    let candidate_auth = build_operator_auth(&new_cfg, &library_for_auth).await?;
    // Defense in depth: never publish a non-loopback listen with auth disabled
    // in the *runtime* object (validate_daemon_listen already checked config).
    validate_daemon_listen_against_auth(&new_cfg, candidate_auth.enabled)?;

    let candidate_destinations = {
        let lib_db = library_for_auth.db();
        bookclerk_plugin_host::load_external_destinations(&new_cfg, Some(lib_db)).await?
    };

    let candidate_sources = crate::registry::default_registry_with_plugins(&new_cfg).await?;
    let candidate_integrations = bookclerk_plugin_host::load_integrations(&new_cfg).await?;

    let token_changed = candidate_auth.enabled != old_auth_enabled
        || (candidate_auth.enabled && candidate_auth.token != old_token);
    let listen_changed = old_listen != new_cfg.daemon.listen;

    // Fail closed before publishing when the new listen set cannot bind at all
    // (ports we already hold are skipped — those need a post-rebind rollback).
    if listen_changed {
        preflight_listen_bind(&old_listen, &new_cfg.daemon.listen).await?;
    }

    // Process-global media pool: only after preflight so a failed reload cannot
    // leave workers on the candidate `[media]` while AppState stays on the old one.
    bookclerk_media::init_pool_from_config(&new_cfg.media);

    // Publish: stop old integrations, swap all slots, then notify listen.
    {
        let old_integrations = {
            let mut guard = state.integrations.write().await;
            std::mem::replace(&mut *guard, candidate_integrations)
        };
        old_integrations.stop_all().await;

        *state.sources.write().await = candidate_sources;
        *state.destinations.write().await = candidate_destinations;
        if let (Some(registry), Some(library)) = (candidate_db_registry, candidate_library) {
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
        *state.config.write().await = new_cfg.clone();
    }

    // Start watchers for the new integration set (awaited — no untracked race).
    start_integration_watchers(state).await;

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
        detail.push_str(&format!(
            "; switched database plugin `{old_db_plugin}` → `{}`",
            new_cfg.database.plugin
        ));
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

/// Validate listen against the effective runtime auth flag (not only TOML).
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

    valid_scoped_key(key, "sources.")
        || valid_scoped_key(key, "integrations.")
        || valid_scoped_key(key, "output.")
        || valid_scoped_key(key, "database.")
}

/// Validates and canonicalizes a Settings value; returns an error string for invalid input.
fn normalize_setting_value(key: &str, value: &str) -> Result<String, String> {
    match key {
        "library.scan_interval_minutes" => value
            .parse::<u64>()
            .map(|_| value.to_string())
            .map_err(|_| "library.scan_interval_minutes must be a non-negative integer".into()),
        "jobs.max_pending" | "jobs.max_attempts" | "jobs.concurrency.network" => value
            .parse::<u32>()
            .map(|n| n.max(1).to_string())
            .map_err(|_| format!("{key} must be a positive integer")),
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
    for source in config.sources.plugins.keys() {
        settings.insert(
            format!("sources.{source}.enabled"),
            config.sources.is_enabled(source).to_string(),
        );
    }
    for (id, value) in &config.sources.plugins {
        let Some(table) = value.as_table() else {
            continue;
        };
        for (key, entry) in table {
            let value_text = entry
                .as_str()
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| entry.to_string());
            settings.insert(format!("sources.{id}.{key}"), value_text);
        }
    }
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

/// True when `id` is the active plugin for `kind` in the current config.
fn plugin_enabled(config: &Config, kind: bookclerk_plugin_host::PluginKind, id: &str) -> bool {
    match kind {
        bookclerk_plugin_host::PluginKind::Source => config.sources.is_enabled(id),
        bookclerk_plugin_host::PluginKind::Integration => config.integrations.is_enabled(id),
        bookclerk_plugin_host::PluginKind::Output if id == "s3" => config.output.s3.enabled,
        bookclerk_plugin_host::PluginKind::Output if id == "local" => config.output.local.enabled,
        bookclerk_plugin_host::PluginKind::Output => false,
        bookclerk_plugin_host::PluginKind::Database => {
            config.database.plugin.eq_ignore_ascii_case(id)
        }
    }
}

/// Dotted `config.toml` prefix for a plugin (`sources.id`, `database.id`, …).
fn plugin_prefix(kind: bookclerk_plugin_host::PluginKind, id: &str) -> String {
    match kind {
        bookclerk_plugin_host::PluginKind::Source => format!("sources.{id}"),
        bookclerk_plugin_host::PluginKind::Integration => format!("integrations.{id}"),
        bookclerk_plugin_host::PluginKind::Output => format!("output.{id}"),
        bookclerk_plugin_host::PluginKind::Database => format!("database.{id}"),
    }
}

/// Wire label for a plugin kind (`source`, `integration`, `output`, `database`).
fn plugin_kind_label(kind: bookclerk_plugin_host::PluginKind) -> &'static str {
    match kind {
        bookclerk_plugin_host::PluginKind::Source => "source",
        bookclerk_plugin_host::PluginKind::Integration => "integration",
        bookclerk_plugin_host::PluginKind::Output => "output",
        bookclerk_plugin_host::PluginKind::Database => "database",
    }
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
                plugin.manifest.kind.as_str(),
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
    }
}

/// Prepends an empty-value Default choice to a fixed list of id/label pairs.
fn plugin_choices_with_default(
    default_label: &str,
    values: impl IntoIterator<Item = (&'static str, &'static str)>,
) -> Vec<PluginSettingChoice> {
    let mut out = vec![plugin_setting_choice("", default_label)];
    out.extend(
        values
            .into_iter()
            .map(|(value, label)| plugin_setting_choice(value, label)),
    );
    out
}

/// Hard-coded Settings options for first-party plugins (bitrate, container, S3, D1, …).
fn built_in_plugin_settings(
    config: &Config,
    kind: bookclerk_plugin_host::PluginKind,
    id: &str,
) -> Vec<PluginSettingOption> {
    let prefix = plugin_prefix(kind, id);
    match (kind, id) {
        (bookclerk_plugin_host::PluginKind::Source, "audible") => {
            vec![plugin_setting_option_with_choices(
                format!("{prefix}.bitrate"),
                "Bitrate",
                config
                    .sources
                    .get_string("audible", "bitrate")
                    .unwrap_or_default(),
                "string",
                plugin_choices_with_default("Default", [("high", "High"), ("normal", "Normal")]),
            )]
        }
        (bookclerk_plugin_host::PluginKind::Source, "libro") => {
            vec![plugin_setting_option_with_choices(
                format!("{prefix}.container"),
                "Container",
                config
                    .sources
                    .get_string("libro", "container")
                    .unwrap_or_default(),
                "string",
                plugin_choices_with_default(
                    "Default",
                    [("m4b", "M4B"), ("zip", "ZIP (MP3 parts)")],
                ),
            )]
        }
        (bookclerk_plugin_host::PluginKind::Source, "graphicaudio") => vec![
            plugin_setting_option_with_choices(
                format!("{prefix}.access"),
                "Access",
                config
                    .sources
                    .get_string("graphicaudio", "access")
                    .unwrap_or_default(),
                "string",
                plugin_choices_with_default(
                    "Default",
                    [
                        ("web", "Browser Player"),
                        ("zip", "Magento ZIP"),
                        ("device", "Access App"),
                    ],
                ),
            ),
            plugin_setting_option_with_choices(
                format!("{prefix}.bitrate"),
                "Bitrate",
                config
                    .sources
                    .get_string("graphicaudio", "bitrate")
                    .unwrap_or_default(),
                "string",
                plugin_choices_with_default("Default", [("hi", "Hi"), ("lo", "Lo")]),
            ),
            plugin_setting_option_with_choices(
                format!("{prefix}.container"),
                "Container",
                config
                    .sources
                    .get_string("graphicaudio", "container")
                    .unwrap_or_default(),
                "string",
                plugin_choices_with_default(
                    "Default",
                    [
                        ("auto", "Auto"),
                        ("m4b", "M4B"),
                        ("mp3", "MP3"),
                        ("flac", "FLAC"),
                    ],
                ),
            ),
        ],
        (bookclerk_plugin_host::PluginKind::Integration, "audiobookshelf") => {
            let cfg = config.integrations.audiobookshelf();
            vec![
                plugin_setting_option(
                    format!("{prefix}.base_url"),
                    "Base URL",
                    cfg.base_url,
                    "string",
                ),
                plugin_setting_option(
                    format!("{prefix}.api_key"),
                    "API Key",
                    cfg.api_key.unwrap_or_default(),
                    "string",
                ),
                plugin_setting_option(
                    format!("{prefix}.library_id"),
                    "Library ID",
                    cfg.library_id.unwrap_or_default(),
                    "string",
                ),
                plugin_setting_option(
                    format!("{prefix}.watch_users"),
                    "Watch Users",
                    cfg.watch_users.to_string(),
                    "boolean",
                ),
                plugin_setting_option(
                    format!("{prefix}.notify_scan_on_acquire"),
                    "Notify Scan On Acquire",
                    cfg.notify_scan_on_acquire.to_string(),
                    "boolean",
                ),
                plugin_setting_option(
                    format!("{prefix}.allow_credential_login"),
                    "Allow Credential Login",
                    cfg.allow_credential_login.to_string(),
                    "boolean",
                ),
            ]
        }
        (bookclerk_plugin_host::PluginKind::Output, "local") => {
            let cfg = &config.output.local;
            vec![
                plugin_setting_option(
                    format!("{prefix}.root"),
                    "Root",
                    cfg.root.to_string_lossy().into_owned(),
                    "string",
                ),
                plugin_setting_option(
                    format!("{prefix}.prefix"),
                    "Prefix",
                    cfg.prefix.clone(),
                    "string",
                ),
                plugin_setting_option_with_choices(
                    format!("{prefix}.naming_profile"),
                    "Naming Profile",
                    cfg.naming
                        .naming_profile
                        .map(bookclerk_config::NamingProfile::as_str)
                        .unwrap_or_default(),
                    "string",
                    plugin_choices_with_default(
                        "Default (global)",
                        bookclerk_config::NamingProfile::all()
                            .iter()
                            .copied()
                            .map(|profile| (profile.as_str(), profile.as_str())),
                    ),
                ),
                plugin_setting_option(
                    format!("{prefix}.folder_template"),
                    "Folder Template",
                    cfg.naming.folder_template.clone().unwrap_or_default(),
                    "string",
                ),
                plugin_setting_option(
                    format!("{prefix}.file_template"),
                    "File Template",
                    cfg.naming.file_template.clone().unwrap_or_default(),
                    "string",
                ),
                plugin_setting_option(
                    format!("{prefix}.chapter_file_template"),
                    "Chapter File Template",
                    cfg.naming.chapter_file_template.clone().unwrap_or_default(),
                    "string",
                ),
            ]
        }
        (bookclerk_plugin_host::PluginKind::Output, "s3") => {
            let cfg = &config.output.s3;
            vec![
                plugin_setting_option(
                    format!("{prefix}.bucket"),
                    "Bucket",
                    cfg.bucket.clone(),
                    "string",
                ),
                plugin_setting_option(
                    format!("{prefix}.prefix"),
                    "Prefix",
                    cfg.prefix.clone(),
                    "string",
                ),
                plugin_setting_option(
                    format!("{prefix}.region"),
                    "Region",
                    cfg.region.clone(),
                    "string",
                ),
                plugin_setting_option(
                    format!("{prefix}.endpoint"),
                    "Endpoint",
                    cfg.endpoint.clone().unwrap_or_default(),
                    "string",
                ),
                plugin_setting_option(
                    format!("{prefix}.force_path_style"),
                    "Force Path Style",
                    cfg.force_path_style.to_string(),
                    "boolean",
                ),
                plugin_setting_option_with_choices(
                    format!("{prefix}.naming_profile"),
                    "Naming Profile",
                    cfg.naming
                        .naming_profile
                        .map(bookclerk_config::NamingProfile::as_str)
                        .unwrap_or_default(),
                    "string",
                    plugin_choices_with_default(
                        "Default (global)",
                        bookclerk_config::NamingProfile::all()
                            .iter()
                            .copied()
                            .map(|profile| (profile.as_str(), profile.as_str())),
                    ),
                ),
                plugin_setting_option(
                    format!("{prefix}.folder_template"),
                    "Folder Template",
                    cfg.naming.folder_template.clone().unwrap_or_default(),
                    "string",
                ),
                plugin_setting_option(
                    format!("{prefix}.file_template"),
                    "File Template",
                    cfg.naming.file_template.clone().unwrap_or_default(),
                    "string",
                ),
                plugin_setting_option(
                    format!("{prefix}.chapter_file_template"),
                    "Chapter File Template",
                    cfg.naming.chapter_file_template.clone().unwrap_or_default(),
                    "string",
                ),
            ]
        }
        (bookclerk_plugin_host::PluginKind::Database, "sqlite") => {
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
        (bookclerk_plugin_host::PluginKind::Database, "d1") => {
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
        (bookclerk_plugin_host::PluginKind::Database, "postgres") => {
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

/// Assembles a Settings group from a live source guest plus its TOML table.
fn build_source_settings_group(
    config: &Config,
    source: &dyn bookclerk_source::ContentSource,
    table: toml::Table,
) -> PluginSettingsGroup {
    let id = source.id();
    let prefix = plugin_prefix(bookclerk_plugin_host::PluginKind::Source, id);
    let mut options = Vec::new();
    let mut seen_keys = std::collections::BTreeSet::new();

    let enabled_key = format!("{prefix}.enabled");
    seen_keys.insert(enabled_key.clone());
    options.push(plugin_setting_option(
        enabled_key,
        "Enabled",
        plugin_enabled(config, bookclerk_plugin_host::PluginKind::Source, id).to_string(),
        "boolean",
    ));

    for option in source.config_options() {
        let key = format!("{prefix}.{}", option.key);
        seen_keys.insert(key.clone());
        let value = table
            .get(option.key)
            .and_then(|entry| entry.as_str().map(ToOwned::to_owned))
            .unwrap_or_default();
        let choices = plugin_choices_with_default(
            "Default",
            option.values.iter().map(|value| (value.id, value.label)),
        );
        options.push(plugin_setting_option_with_choices(
            key,
            option.label,
            value,
            "string",
            choices,
        ));
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
        kind: plugin_kind_label(bookclerk_plugin_host::PluginKind::Source).to_string(),
        logo: non_empty_logo(source.portal_brand().icon_url),
        settings: options,
    }
}

/// Assembles a Settings group for an integration, overlaying portal-brand logo when present.
fn build_integration_settings_group(
    config: &Config,
    integration: &dyn bookclerk_integrations::Integration,
    table: toml::Table,
) -> PluginSettingsGroup {
    let id = integration.id();
    let mut group = build_plugin_settings_group(
        config,
        bookclerk_plugin_host::PluginKind::Integration,
        id,
        table,
    );
    if let Some(brand) = integration.portal_brand() {
        group.logo = non_empty_logo(brand.icon_url);
    }
    group
}

/// Assembles a generic Settings group (enabled + built-ins + leftover TOML keys).
fn build_plugin_settings_group(
    config: &Config,
    kind: bookclerk_plugin_host::PluginKind,
    id: &str,
    table: toml::Table,
) -> PluginSettingsGroup {
    let prefix = plugin_prefix(kind, id);
    let mut options = Vec::new();
    let mut seen_keys = std::collections::BTreeSet::new();

    match kind {
        bookclerk_plugin_host::PluginKind::Database => {
            let key = format!("database.{id}.enabled");
            seen_keys.insert(key.clone());
            options.push(plugin_setting_option(
                key,
                "Enabled",
                plugin_enabled(config, kind, id).to_string(),
                "boolean",
            ));
        }
        _ => {
            let key = format!("{prefix}.enabled");
            seen_keys.insert(key.clone());
            options.push(plugin_setting_option(
                key,
                "Enabled",
                plugin_enabled(config, kind, id).to_string(),
                "boolean",
            ));
        }
    }

    for option in built_in_plugin_settings(config, kind, id) {
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
        kind: plugin_kind_label(kind).to_string(),
        logo: None,
        settings: options,
    }
}

/// Merges discovered and in-process plugins into Settings groups keyed by kind and id.
fn plugin_settings_snapshot(
    config: &Config,
    sources: &SourceRegistry,
    integrations: &IntegrationRegistry,
    discovered_plugins: &[bookclerk_plugin_host::DiscoveredPlugin],
) -> Vec<PluginSettingsGroup> {
    let mut groups_by_key: std::collections::BTreeMap<(String, String), PluginSettingsGroup> =
        std::collections::BTreeMap::new();

    for plugin in discovered_plugins {
        let table = bookclerk_plugin_host::settings_table(config, plugin);
        let mut group = match plugin.manifest.kind {
            bookclerk_plugin_host::PluginKind::Source => {
                if let Some(source) = sources.get(&plugin.manifest.id) {
                    build_source_settings_group(config, source.as_ref(), table)
                } else {
                    build_plugin_settings_group(
                        config,
                        plugin.manifest.kind,
                        &plugin.manifest.id,
                        table,
                    )
                }
            }
            bookclerk_plugin_host::PluginKind::Integration => {
                if let Some(integration) = integrations.get(&plugin.manifest.id) {
                    build_integration_settings_group(config, integration.as_ref(), table)
                } else {
                    build_plugin_settings_group(
                        config,
                        plugin.manifest.kind,
                        &plugin.manifest.id,
                        table,
                    )
                }
            }
            _ => build_plugin_settings_group(
                config,
                plugin.manifest.kind,
                &plugin.manifest.id,
                table,
            ),
        };
        // Prefer live BrandDto / portal brand (already on group); else plugin.toml logo.
        if group.logo.is_none() {
            if let Some(logo) = settings_logo_from_manifest(plugin) {
                group.logo = Some(logo);
            }
        }
        groups_by_key.insert((group.kind.clone(), group.id.clone()), group);
    }

    // Loaded in-process guests that were not also discovered from disk.
    for source in sources.all() {
        let id = source.id().to_string();
        let key = (String::from("source"), id.clone());
        if !groups_by_key.contains_key(&key) {
            let table = config.sources.table(&id).cloned().unwrap_or_default();
            let group = build_source_settings_group(config, source.as_ref(), table);
            groups_by_key.insert((group.kind.clone(), group.id.clone()), group);
        }
    }
    for integration in integrations.all() {
        let id = integration.id().to_string();
        let key = (String::from("integration"), id.clone());
        if !groups_by_key.contains_key(&key) {
            let table = config
                .integrations
                .plugin_table(&id)
                .cloned()
                .unwrap_or_default();
            let group = build_integration_settings_group(config, integration.as_ref(), table);
            groups_by_key.insert((group.kind.clone(), group.id.clone()), group);
        }
    }

    groups_by_key.into_values().collect()
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
        if config.database.plugin.eq_ignore_ascii_case(&id) {
            config.database.plugin.clear();
            break;
        }
    }
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
        if !matches!(parts[0], "sources" | "integrations" | "output" | "database") {
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
            if !current_database_plugin.eq_ignore_ascii_case(id) {
                ids.push(id.to_string());
            }
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
        if !current_database_plugin.eq_ignore_ascii_case(id) {
            ids.push(id.to_string());
        }
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

/// Discover `kind`/`id` and read an embedded logo (sync; route + integration tests).
fn plugin_logo_bytes_for(
    config: &Config,
    kind: &str,
    id: &str,
) -> Result<(Vec<u8>, &'static str), StatusCode> {
    let plugin = bookclerk_plugin_host::discover_plugins(config)
        .map_err(|_| StatusCode::NOT_FOUND)?
        .into_iter()
        .find(|p| plugin_kind_label(p.manifest.kind) == kind && p.manifest.id == id)
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
    match plugin.manifest.kind {
        bookclerk_plugin_host::PluginKind::Source => {
            let source = state.sources.read().await.get(&plugin.manifest.id);
            if let Some(source) = source {
                let source_brand = source.portal_brand();
                brand.name = source_brand.name.to_string();
                brand.bg = Some(source_brand.bg.to_string());
                brand.fg = Some(source_brand.fg.to_string());
                brand.accent = Some(source_brand.accent.to_string());
                brand.logo = non_empty_logo(source_brand.logo_href()).or(brand.logo);
            }
        }
        bookclerk_plugin_host::PluginKind::Integration => {
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
        bookclerk_plugin_host::PluginKind::Output | bookclerk_plugin_host::PluginKind::Database => {
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

/// Loads the consent dialog for one plugin, including coverage against the stored grant.
async fn get_plugin_consent(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<PluginConsentResponse>, StatusCode> {
    let cfg = state.config.read().await.clone();
    let plugin = discover_plugins_for_settings(&cfg)
        .await
        .into_iter()
        .find(|p| p.manifest.id == id)
        .ok_or(StatusCode::NOT_FOUND)?;
    let request = consent_request(&plugin.manifest);
    let summary = consent_summary(&request);
    let store = PluginGrantStore::load(&cfg.paths().files_dir).map_err(|err| {
        tracing::error!(error = %err, "failed to load plugin grants");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let existing = store.get(&id).cloned();
    let covered = existing
        .as_ref()
        .is_some_and(|grant| grant_covers(grant, &request));
    let brand = plugin_consent_brand(&state, &plugin).await;
    let limits = plugin_consent_limits(&plugin, cfg.plugins.jail.cpu_rate_percent);
    let runtime = match plugin.manifest.runtime {
        PluginRuntimeKind::Native => "native",
        PluginRuntimeKind::Workerd => "workerd",
    }
    .to_string();
    Ok(Json(PluginConsentResponse {
        plugin_id: id,
        runtime,
        request: PluginGrantView::from_grant(&request),
        covered,
        summary,
        existing: existing.as_ref().map(PluginGrantView::from_grant),
        brand,
        limits,
    }))
}

/// Persists an approved plugin grant after validating operator overrides.
async fn post_plugin_consent(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<PluginConsentApproveRequest>,
) -> Result<Json<PluginConsentResponse>, StatusCode> {
    if !body.approve {
        return Err(StatusCode::BAD_REQUEST);
    }
    let cfg = state.config.read().await.clone();
    let plugin = discover_plugins_for_settings(&cfg)
        .await
        .into_iter()
        .find(|p| p.manifest.id == id)
        .ok_or(StatusCode::NOT_FOUND)?;
    let request = consent_request(&plugin.manifest);
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
    let brand = plugin_consent_brand(&state, &plugin).await;
    let limits = plugin_consent_limits(&plugin, cfg.plugins.jail.cpu_rate_percent);
    let runtime = match plugin.manifest.runtime {
        PluginRuntimeKind::Native => "native",
        PluginRuntimeKind::Workerd => "workerd",
    }
    .to_string();
    Ok(Json(PluginConsentResponse {
        plugin_id: id,
        runtime,
        request: PluginGrantView::from_grant(&request),
        covered: true,
        summary,
        existing: Some(PluginGrantView::from_grant(&approved)),
        brand,
        limits,
    }))
}

/// Returns file-backed and runtime-effective Settings plus per-plugin groups.
async fn get_settings(
    State(state): State<Arc<AppState>>,
) -> Result<Json<SettingsResponse>, StatusCode> {
    let cfg = state.config.read().await.clone();
    let discovered_plugins = discover_plugins_for_settings(&cfg).await;
    let sources = state.sources.read().await;
    let integrations = state.integrations.read().await;
    let auth = state.auth_snapshot().await;
    let settings = current_settings_snapshot(&cfg);
    let mut effective = settings.clone();
    effective.insert("daemon.auth.enabled".into(), auth.enabled.to_string());
    effective.insert("daemon.listen".into(), cfg.daemon.listen.join_comma());
    for source in sources.all() {
        effective.insert(
            format!("sources.{}.enabled", source.id()),
            cfg.sources.is_enabled(source.id()).to_string(),
        );
        effective.insert(
            format!("runtime.sources.{}.loaded", source.id()),
            "true".into(),
        );
    }
    for integration in integrations.all() {
        effective.insert(
            format!("integrations.{}.enabled", integration.id()),
            cfg.integrations.is_enabled(integration.id()).to_string(),
        );
        effective.insert(
            format!("runtime.integrations.{}.loaded", integration.id()),
            "true".into(),
        );
    }
    Ok(Json(SettingsResponse {
        settings,
        effective,
        plugins: plugin_settings_snapshot(&cfg, &sources, &integrations, &discovered_plugins),
        host_cpu_cores_max: host_cpu_cores_max(),
        jail_cpu_cores: cfg
            .plugins
            .jail
            .cpu_rate_percent
            .map(|n| effective_cpu_cores(Some(percent_to_cores(n)))),
    }))
}

/// Validates, writes, and reloads allowlisted Settings keys under `reload_lock`.
async fn patch_settings(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PatchSettingsRequest>,
) -> Result<Json<SettingsResponse>, Response> {
    if body.settings.is_empty() {
        return get_settings(State(state.clone()))
            .await
            .map_err(IntoResponse::into_response);
    }

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
    let editable_keys: std::collections::HashSet<&str> = editable
        .plugins
        .iter()
        .flat_map(|plugin| plugin.settings.iter().map(|setting| setting.key.as_str()))
        .chain(editable.settings.keys().map(String::as_str))
        .collect();
    let editable_plugin_options: std::collections::HashMap<&str, &PluginSettingOption> = editable
        .plugins
        .iter()
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
    // must not bypass consent — same bar as `bookclerk plugins enable`.
    for id in database_backends_requiring_grant(&updates, &current_database_plugin) {
        if !enabling.iter().any(|e| e.eq_ignore_ascii_case(&id)) {
            enabling.push(id);
        }
    }
    if !enabling.is_empty() {
        let discovered = discover_plugins_for_settings(
            &Config::load(Some(files_dir.clone()), Some(config_path.clone())).map_err(|err| {
                tracing::error!(error = %err, "failed to load config for consent check");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            })?,
        )
        .await;
        for plugin_id in &enabling {
            let Some(plugin) = discovered.iter().find(|p| p.manifest.id == *plugin_id) else {
                tracing::warn!(%plugin_id, "cannot enable undiscovered plugin");
                return Err(StatusCode::BAD_REQUEST.into_response());
            };
            if let Err(err) = require_grant(&files_dir, &plugin.manifest) {
                let request = consent_request(&plugin.manifest);
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
    for (key, value) in &updates {
        if key.starts_with("database.") && key.ends_with(".enabled") {
            // Exclusive enablement is applied via `apply_database_enable_updates`.
            continue;
        }
        normalized_pairs.push((key.clone(), value.clone()));
    }

    let _reload_guard = state.reload_lock.lock().await;

    let mut cfg = Config::load(Some(files_dir), Some(config_path.clone())).map_err(|err| {
        tracing::error!(error = %err, "failed to load config for settings update");
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    })?;

    apply_database_enable_updates(&mut cfg, &updates).map_err(|err| {
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
                let Some(plugin) = discovered
                    .iter()
                    .find(|p| p.manifest.id.eq_ignore_ascii_case(&to_plugin))
                else {
                    tracing::warn!(%to_plugin, "cannot apply migrate to undiscovered database plugin");
                    return Err(StatusCode::BAD_REQUEST);
                };
                if let Err(err) = require_grant(&files_dir, &plugin.manifest) {
                    tracing::warn!(
                        plugin = %to_plugin,
                        error = %err,
                        "database migrate apply blocked pending consent"
                    );
                    return Err(StatusCode::FORBIDDEN);
                }
                let path = apply_migrated_database_plugin(&state, to_plugin.clone()).await?;
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
async fn status(State(state): State<Arc<AppState>>) -> Result<Json<StatusResponse>, StatusCode> {
    let library = state.library_snapshot().await;
    let accounts = library
        .count_accounts()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)? as usize;
    let books = library
        .count_books(None)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)? as usize;
    let acquired = library
        .count_by_status(AcquireStatus::Acquired)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let pending = library
        .count_by_status(AcquireStatus::NotAcquired)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let error = library
        .count_by_status(AcquireStatus::Error)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let queued = library
        .count_by_status(AcquireStatus::Queued)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let downloading = library
        .count_by_status(AcquireStatus::Downloading)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let in_progress = queued + downloading;
    let (listen, storage_backend) = {
        let cfg = state.config.read().await;
        let names = cfg.output.enabled_backend_names();
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
    }))
}

/// Enqueues a library scan job and returns the accepted job id.
async fn trigger_scan(
    State(state): State<Arc<AppState>>,
    body: Option<Json<ScanRequest>>,
) -> impl IntoResponse {
    let account = body.and_then(|Json(b)| b.account);
    match enqueue_scan(state, account, JobTrigger::Api).await {
        Ok(admit) => admit_response("scan", admit),
        Err(err) => enqueue_error_response(err),
    }
}

/// Enqueues an acquire job, optionally filtered by title id and account.
async fn trigger_acquire(
    State(state): State<Arc<AppState>>,
    body: Option<Json<AcquireRequestBody>>,
) -> impl IntoResponse {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let title_filter = body.uuid.or(body.asin).or(body.isbn).or(body.product_id);
    match enqueue_acquire(state, title_filter, body.account, JobTrigger::Api).await {
        Ok(admit) => admit_response("acquire", admit),
        Err(err) => enqueue_error_response(err),
    }
}

/// Maps a durable job row onto the control-plane `JobInfo` JSON.
fn job_info_from_record(job: JobRecord) -> JobInfo {
    let detail = job.progress.clone().or(job.error_message.clone());
    JobInfo {
        id: job.id,
        kind: job.kind.as_str().to_string(),
        status: job.state.as_str().to_string(),
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

/// Returns HTTP 503 when admission is paused, otherwise 500.
fn enqueue_error_response(err: anyhow::Error) -> Response {
    tracing::error!(error = %err, "job enqueue failed");
    let paused = err.to_string().contains("admission paused");
    let status = if paused {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    let error = if paused {
        "admission_paused"
    } else {
        "internal_error"
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
async fn list_jobs(State(state): State<Arc<AppState>>) -> Result<Json<Vec<JobInfo>>, StatusCode> {
    let library = state.library_snapshot().await;
    let jobs = library
        .list_jobs(100)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(jobs.into_iter().map(job_info_from_record).collect()))
}

/// Cancel a pending job or request cooperative stop of a running job.
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

/// Enqueues a remote integration library scan; 404 if missing, 400 if unsupported.
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
async fn list_books(
    State(state): State<Arc<AppState>>,
    _headers: HeaderMap,
    Query(query): Query<BooksQuery>,
) -> Result<Json<BooksResponse>, StatusCode> {
    let library = state.library_snapshot().await;
    let limit = query.limit.unwrap_or(40).clamp(1, 500);
    let offset = query.offset.unwrap_or(0);
    let status_filter = query.status.as_deref().and_then(AcquireStatus::parse);

    // Authenticated callers (operator or user) share the full library for
    // browsing. Account *linking* and acquire remain capability-gated; store
    // connect is User-only.

    let mut books = if let Some(q) = query.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let index_dir = state.config.read().await.paths().search_index_dir.clone();
        // Offloaded: Tantivy query work would otherwise block this request task.
        let hits = SearchEngine::open_and_search(index_dir, q.to_string(), 500)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let mut out = Vec::new();
        for hit in hits {
            if let Some(account) = query.account.as_deref() {
                if hit.account_id != account {
                    continue;
                }
            }
            if let Some(book) = book_for_search_hit(&library, &hit)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            {
                out.push(book);
            }
        }
        out
    } else if let Some(account) = query.account.as_deref() {
        library
            .list_books(Some(account))
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    } else {
        library
            .list_books(None)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    };

    if let Some(status) = status_filter {
        books.retain(|b| b.acquire_status == status);
    }

    books.sort_by(|a, b| {
        a.title
            .to_ascii_lowercase()
            .cmp(&b.title.to_ascii_lowercase())
            .then_with(|| a.uuid.cmp(&b.uuid))
    });

    let total = books.len();
    let page = books.into_iter().skip(offset).take(limit).collect();
    Ok(Json(BooksResponse {
        books: page,
        total,
        limit,
        offset,
    }))
}

/// Returns one title by UUID, or 404 when it is absent.
async fn get_book(
    State(state): State<Arc<AppState>>,
    AxumPath(uuid): AxumPath<String>,
) -> Result<Json<BookRecord>, StatusCode> {
    let library = state.library_snapshot().await;
    library
        .get_book_by_uuid(&uuid)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// Serves the local JPEG cover sidecar for a title, or 404 when local output is off.
async fn get_book_cover(
    State(state): State<Arc<AppState>>,
    AxumPath(uuid): AxumPath<String>,
) -> Result<Response, StatusCode> {
    let library = state.library_snapshot().await;
    let book = library
        .get_book_by_uuid(&uuid)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let storage_key = book.storage_key.as_deref().ok_or(StatusCode::NOT_FOUND)?;
    let cfg = state.config.read().await;
    if !cfg.output.local.enabled {
        return Err(StatusCode::NOT_FOUND);
    }
    let cover_key = sidecar_key(storage_key, "jpg");
    let path = resolve_local_key(&cfg.output.local.root, &cfg.output.local.prefix, &cover_key);
    if !path.is_file() {
        return Err(StatusCode::NOT_FOUND);
    }
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "image/jpeg")],
        bytes,
    )
        .into_response())
}

/// Joins local output root, prefix, and object key while dropping `.` / `..` segments.
fn resolve_local_key(root: &Path, prefix: &str, key: &str) -> PathBuf {
    let mut path = root.to_path_buf();
    let prefix = prefix.trim_matches('/');
    if !prefix.is_empty() {
        path.push(prefix);
    }
    for part in key.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            continue;
        }
        path.push(part);
    }
    path
}

/// Resolve a search hit without scanning the full account library.
///
/// The index stores ids lowercased and returns `asin` uppercased for display, so
/// an exact `get_book(&hit.asin)` can miss. Prefer uuid, then a small set of
/// case-normalized title_id candidates.
async fn book_for_search_hit(
    library: &LibraryStore,
    hit: &SearchHit,
) -> Result<Option<BookRecord>, bookclerk_library::LibraryError> {
    if !hit.uuid.is_empty() {
        if let Some(book) = library.get_book_by_uuid(&hit.uuid).await? {
            return Ok(Some(book));
        }
    }
    for candidate in title_id_candidates(&hit.asin) {
        if let Some(book) = library.get_book(&candidate, &hit.account_id).await? {
            return Ok(Some(book));
        }
    }
    Ok(None)
}

/// Yields the original title id plus case-folded variants for search-hit lookup.
fn title_id_candidates(id: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(3);
    if id.is_empty() {
        return out;
    }
    out.push(id.to_string());
    let lower = id.to_ascii_lowercase();
    if lower != id {
        out.push(lower);
    }
    let upper = id.to_ascii_uppercase();
    if upper != id && out.iter().all(|c| c != &upper) {
        out.push(upper);
    }
    out
}

#[derive(Debug, Deserialize, Default)]
/// Query string for `GET /api/discover/recommendations`.
struct RecommendQuery {
    /// Maximum recommendations to return.
    limit: Option<usize>,
    /// Optional external user id used to bias the feed.
    user: Option<String>,
    #[serde(default)]
    /// When true, skip storefront price/availability enrichment.
    no_purchase_hints: Option<bool>,
    /// When true, ignore listening_progress (owned-library taste only).
    #[serde(default)]
    no_listening: Option<bool>,
    /// Comma-separated integration ids; empty = all listening providers.
    #[serde(default)]
    listening_providers: Option<String>,
}

#[derive(Debug, Deserialize)]
/// Wishlist / title-request create body from Discover or the portal.
struct CreateRequestBody {
    /// Required title text; empty values are rejected with 400.
    title: String,
    /// Optional author list as a single display string.
    authors: Option<String>,
    /// Optional Audible ASIN used for work-key identity.
    asin: Option<String>,
    /// Optional ISBN used for work-key identity.
    isbn: Option<String>,
    /// Optional operator or member note stored on the request.
    notes: Option<String>,
    /// Known storefront editions to persist as `title_request_sources`.
    #[serde(default)]
    store_editions: Vec<bookclerk_discover::StoreEdition>,
    /// Optional priced storefront links snapshotted at wishlist time.
    #[serde(default)]
    purchase_hints: Vec<bookclerk_discover::PurchaseHint>,
    /// Optional stable work key; when omitted one is derived from bib fields.
    work_key: Option<String>,
    /// Optional cover image URL snapshotted onto the request.
    cover_url: Option<String>,
    /// Optional plot/summary text persisted with the request.
    description: Option<String>,
    /// Optional subtitle stored on the request and source rows.
    subtitle: Option<String>,
    /// Optional narrator list as a single display string.
    narrators: Option<String>,
    /// Optional series name.
    series: Option<String>,
    /// Optional series position as a display string.
    series_index: Option<String>,
    /// Optional publisher name.
    publisher: Option<String>,
    /// Optional runtime in minutes.
    length_minutes: Option<i64>,
    /// Optional publication date as a display string.
    published_at: Option<String>,
    /// Optional genre list as a single display string.
    genres: Option<String>,
    /// Optional content language code.
    language: Option<String>,
}

#[derive(Debug, Deserialize)]
/// Query string for multi-store Discover catalog search.
struct CatalogSearchQuery {
    /// Search text; queries shorter than two characters return an empty page.
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

/// Builds the Discover recommendation feed, skipping embed work on an empty library.
async fn discover_recommendations(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<RecommendQuery>,
) -> Result<Json<bookclerk_discover::DiscoverFeed>, (StatusCode, String)> {
    let cfg = state.config.read().await.clone();
    let library = state.library_snapshot().await;

    // Fast path: skip embedding/recommendation work when there is no library or
    // wishlist data yet. This keeps initial GUI loads responsive in empty setups.
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
        }));
    }

    // Wishlist-only (no library books): skip ONNX warm-up. MiniLM load routinely
    // exceeds the 8s API timeout, and with no owned titles there is no taste
    // centroid / storefront seed work that needs real embeddings.
    let use_onnx = has_books && cfg.discovery.embeddings_enabled;

    if has_books {
        let _ = bookclerk_discover::rebuild_works_from_library(&library)
            .await
            .map_err(internal_err)?;
    }

    let _discover = state
        .discover_gate
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "discover gate closed".into(),
            )
        })?;

    let model_id = if use_onnx {
        let mut embedder = open_embedder_blocking(
            cfg.paths().models_dir.clone(),
            cfg.discovery.embed_intra_threads,
            true,
        )
        .await?;
        let model_id = embedder.model_id().to_string();
        let _ = bookclerk_discover::embed_dirty_works(&library, embedder.as_mut()).await;
        model_id
    } else {
        String::from(bookclerk_discover::MODEL_LOCAL_HASH_V1)
    };

    let listening_providers = q
        .listening_providers
        .as_deref()
        .unwrap_or("")
        .split([',', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();

    // Portal sessions personalize by default from their external user id.
    let external_user_id = if let Some(user) = q.user {
        Some(user)
    } else if let Some(identity) = auth::caller_portal_identity(&state, &headers).await {
        Some(identity.external_user_id)
    } else {
        None
    };

    let (subject_key, identity_id) = auth::prefs_subject_for_caller(&state, &headers)
        .await
        .map_err(|status| (status, status.canonical_reason().unwrap_or("error").into()))?;
    let disabled_shelves = library
        .get_user_preferences_or_default(&subject_key, identity_id)
        .await
        .map(|p| p.disabled_shelves)
        .unwrap_or_default();

    let opts = bookclerk_discover::RecommendOptions {
        limit: q.limit.unwrap_or(cfg.discovery.recommend_limit.max(24)),
        embedding_model: model_id,
        region: String::from("us"),
        include_purchase_hints: !q.no_purchase_hints.unwrap_or(false),
        external_user_id,
        include_listening: !q.no_listening.unwrap_or(false),
        listening_providers,
        fetch_storefront_candidates: has_books && cfg.discovery.storefront_candidates,
        storefront_seed_limit: cfg.discovery.storefront_seed_limit,
        storefront_max_remote_calls: cfg.discovery.storefront_max_remote_calls,
        exclude_graphicaudio_series_sets: cfg.discovery.exclude_graphicaudio_series_sets,
        disabled_shelves,
        models_dir: use_onnx.then(|| cfg.paths().models_dir.clone()),
        embed_intra_threads: cfg.discovery.embed_intra_threads,
        embeddings_enabled: use_onnx,
    };
    let sources = state.sources.read().await;
    let mut feed = bookclerk_discover::recommend_feed(&library, &sources, &opts)
        .await
        .map_err(internal_err)?;
    let files = crate::profile::files_dir(&state).await;
    for shelf in &mut feed.shelves {
        for item in &mut shelf.items {
            hydrate_queue_wishers(&mut item.wishers, files.as_deref());
        }
    }
    Ok(Json(feed))
}

/// Resolves storefront price/availability for one title, forcing server-side preferred stores.
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

#[derive(Debug, serde::Deserialize)]
/// Body for batched purchase-hint lookups (max 24 queries).
struct PurchaseHintsBatchBody {
    #[serde(default)]
    /// Per-title purchase-hint queries in this batch.
    queries: Vec<bookclerk_discover::PurchaseHintsQuery>,
}

#[derive(Debug, serde::Serialize)]
/// Parallel purchase-hint results aligned with the request queries.
struct PurchaseHintsBatchResponse {
    /// One purchase-hint response per query, in request order.
    results: Vec<bookclerk_discover::PurchaseHintsResponse>,
}

/// Resolves up to 24 purchase-hint queries with caller-derived preferred stores.
async fn discover_purchase_hints_batch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<PurchaseHintsBatchBody>,
) -> Result<Json<PurchaseHintsBatchResponse>, (StatusCode, String)> {
    if body.queries.len() > 24 {
        return Err((
            StatusCode::BAD_REQUEST,
            "at most 24 purchase-hint queries per batch".into(),
        ));
    }
    let preferred = preferred_sources_for_caller(&state, &headers).await;
    let mut queries = Vec::with_capacity(body.queries.len());
    for mut q in body.queries {
        validate_purchase_hints_query(&q)?;
        q.preferred_sources = preferred.clone();
        queries.push(q);
    }
    let resolved = {
        let sources = state.sources.read().await;
        bookclerk_discover::resolve_purchase_hints_batch(&sources, &queries, 4).await
    };
    let mut results = Vec::with_capacity(resolved.len());
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
async fn discover_title_meta(
    State(state): State<Arc<AppState>>,
    Json(body): Json<bookclerk_discover::TitleMetaQuery>,
) -> Result<Json<Option<bookclerk_discover::TitleMeta>>, (StatusCode, String)> {
    validate_title_meta_query(&body)?;
    let sources = state.sources.read().await;
    let meta = bookclerk_discover::resolve_title_meta(&body, Some(&sources))
        .await
        .map_err(internal_err)?;
    Ok(Json(meta))
}

/// Fetches Audnexus reviews for an ASIN; rejects an empty ASIN with 400.
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

#[derive(Debug, serde::Deserialize)]
/// Body for batched title-metadata lookups (max 24 queries).
struct TitleMetaBatchBody {
    #[serde(default)]
    /// Per-title metadata queries in this batch.
    queries: Vec<bookclerk_discover::TitleMetaQuery>,
}

#[derive(Debug, serde::Serialize)]
/// Title-metadata results aligned with the request; failed items are `null`.
struct TitleMetaBatchResponse {
    /// One optional metadata payload per query; enrichment errors become `None`.
    results: Vec<Option<bookclerk_discover::TitleMeta>>,
}

/// Resolves up to 24 title-metadata queries without failing the whole batch on one error.
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
        let sources = state.sources.read().await;
        bookclerk_discover::resolve_title_meta_batch(&body.queries, 4, Some(&sources)).await
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
async fn discover_catalog_search(
    State(state): State<Arc<AppState>>,
    Query(q): Query<CatalogSearchQuery>,
) -> Result<Json<bookclerk_discover::CatalogSearchPage>, (StatusCode, String)> {
    // Wall budget for parallel multi-store search (each store capped ~7s;
    // optional Libro page enrich ~3.5s; over-fetch may need a few rounds).
    const CATALOG_SEARCH_TIMEOUT: Duration = Duration::from_secs(15);
    let query = q.q.unwrap_or_default();
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

/// Returns the client work key, or derives one from ASIN/ISBN/title/edition identity.
fn work_key_for_request(body: &CreateRequestBody) -> String {
    if let Some(key) = body
        .work_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return key.to_string();
    }
    let (source, product_id) = body
        .store_editions
        .first()
        .map(|e| (Some(e.source.as_str()), Some(e.product_id.as_str())))
        .unwrap_or((None, None));
    bookclerk_discover::work_map_key(
        body.asin.as_deref(),
        body.isbn.as_deref(),
        &body.title,
        body.authors.as_deref(),
        source,
        product_id.or(body.asin.as_deref()).or(body.isbn.as_deref()),
    )
}

/// Lists open wishlist rows for the caller (portal identity or operator-owned).
async fn list_wishlist(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<TitleRequestRecord>>, (StatusCode, String)> {
    let identity_id = auth::caller_portal_identity(&state, &headers)
        .await
        .map(|identity| identity.id);
    let library = state.library_snapshot().await;
    let rows = library
        .list_wishlist(identity_id)
        .await
        .map_err(internal_err)?;
    Ok(Json(rows))
}

/// Creates a wishlist / title-request row for the authenticated caller.
async fn create_wishlist(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<CreateRequestBody>,
) -> Result<Json<TitleRequestRecord>, (StatusCode, String)> {
    create_request_inner(&state, &headers, body).await
}

/// Cancels an open wishlist item the caller owns; others get 403.
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
    library
        .get_title_request_by_uuid(&uuid)
        .await
        .map_err(internal_err)?
        .map(Json)
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("wishlist item not found: {uuid}"),
        ))
}

/// Marks whether each wisher has an uploaded avatar under `$BOOKCLERK_FILES_DIR`.
fn hydrate_queue_wishers(wishers: &mut [QueueWisher], files: Option<&Path>) {
    for wisher in wishers {
        if let Some(id) = wisher.user_id {
            wisher.has_avatar = files.is_some_and(|d| crate::profile::avatar_exists(d, id));
            wisher.avatar_source = Some(crate::profile::avatar_source_wire(
                wisher.avatar_source.as_deref(),
            ));
        }
    }
}

/// Ranks the shared open-request queue without warming ONNX embeddings.
async fn list_request_queue(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<bookclerk_discover::RankedQueueEntry>>, (StatusCode, String)> {
    let cfg = state.config.read().await.clone();
    let library = state.library_snapshot().await;

    // Empty shared queue: skip discover_gate + ONNX. Opening the embedder alone
    // often exceeds the 8s API timeout and surfaces as a Wishlist 504.
    let open_requests = library
        .list_title_requests(Some(RequestStatus::Open))
        .await
        .map_err(internal_err)?;
    if open_requests.is_empty() {
        return Ok(Json(Vec::new()));
    }

    let _discover = state
        .discover_gate
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "discover gate closed".into(),
            )
        })?;

    // Do not warm ONNX here. Wish-count already dominates ranking, and MiniLM
    // cold-start alone blows the global 8s API timeout (Wishlist 504 after the
    // first title is wishlisted). Author/category taste still applies without
    // embeddings when a library exists.
    let opts = bookclerk_discover::RecommendOptions {
        limit: cfg.discovery.recommend_limit.max(24),
        embedding_model: String::from(bookclerk_discover::MODEL_LOCAL_HASH_V1),
        region: String::from("us"),
        include_purchase_hints: false,
        external_user_id: None,
        include_listening: true,
        listening_providers: Vec::new(),
        fetch_storefront_candidates: false,
        storefront_seed_limit: 0,
        storefront_max_remote_calls: 0,
        exclude_graphicaudio_series_sets: cfg.discovery.exclude_graphicaudio_series_sets,
        disabled_shelves: Vec::new(),
        models_dir: None,
        embed_intra_threads: cfg.discovery.embed_intra_threads,
        embeddings_enabled: false,
    };
    let sources = state.sources.read().await;
    let mut rows = bookclerk_discover::rank_global_request_queue(&library, &sources, &opts)
        .await
        .map_err(internal_err)?;
    let files = crate::profile::files_dir(&state).await;
    for row in &mut rows {
        hydrate_queue_wishers(&mut row.wishers, files.as_deref());
    }
    Ok(Json(rows))
}

/// Inserts a title request after requiring a non-empty title and deriving a work key.
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
    let work_key = work_key_for_request(&body);
    let library = state.library_snapshot().await;
    let mut row = library
        .create_title_request(&NewTitleRequest {
            uuid: None,
            identity_id,
            title: body.title.clone(),
            authors: body.authors.clone(),
            asin: body.asin.clone(),
            isbn: body.isbn.clone(),
            notes: body.notes.clone(),
            status: RequestStatus::Open,
            work_key,
            work_id: None,
            resolved_book_uuid: None,
            cover_url: body.cover_url.clone(),
        })
        .await
        .map_err(internal_err)?;

    let mut sources = wishlist_sources_from_body(&body);
    // Soft-resolve live prices / additional storefronts and merge into snapshots.
    let hint_query = bookclerk_discover::PurchaseHintsQuery {
        title: body.title.clone(),
        authors: body.authors.clone(),
        asin: body.asin.clone(),
        isbn: body.isbn.clone(),
        candidate_source: None,
        candidate_product_id: None,
        store_editions: body.store_editions.clone(),
        region: Some(String::from("us")),
        preferred_sources: Vec::new(),
    };
    {
        let registry = state.sources.read().await;
        match bookclerk_discover::resolve_purchase_hints(&registry, &hint_query).await {
            Ok(resolved) => {
                for hint in resolved.hints {
                    sources.push(source_from_purchase_hint(&hint, &body));
                }
            }
            Err(err) => {
                tracing::debug!(error = %err, "wishlist purchase-hint resolve soft-failed");
            }
        }
    }

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
    }
}

/// Enqueues a listening-progress sync; 400 when no capable integrations are loaded.
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

#[derive(Debug, Serialize)]
/// Normalized Discover/library preferences for the authenticated subject.
struct PreferencesResponse {
    /// Landing view id (`library`, `discover`, …) after normalization.
    default_view: String,
    /// Lowercased Discover shelf ids the user has hidden.
    disabled_shelves: Vec<String>,
    /// Discover sort key (`relevance`, `popularity`, …).
    discover_sort: String,
    /// Discover sort direction (`asc` or `desc`).
    discover_sort_dir: String,
    /// `null` = use browser language on the client.
    discover_language: Option<String>,
    /// Lowercased store ids hidden from Discover results.
    discover_excluded_sources: Vec<String>,
    /// Appearance preference (`system`, `light`, or `dark`).
    theme: String,
}

#[derive(Debug, Deserialize)]
/// Partial preferences update; omitted fields keep their stored values.
struct PatchPreferencesBody {
    /// Replacement landing view when set.
    default_view: Option<String>,
    /// Replacement hidden-shelf list when set.
    disabled_shelves: Option<Vec<String>>,
    /// Replacement Discover sort key when set.
    discover_sort: Option<String>,
    /// Replacement Discover sort direction when set.
    discover_sort_dir: Option<String>,
    /// Present + value sets language; present + `null` clears to browser default;
    /// omitted leaves unchanged.
    #[serde(default, deserialize_with = "deserialize_patch_opt_string")]
    discover_language: Option<Option<String>>,
    /// Replacement excluded-store list when set.
    discover_excluded_sources: Option<Vec<String>>,
    /// Replacement appearance preference when set (`system`, `light`, `dark`).
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
    use super::{
        allowed_setting_key, apply_database_enable_updates, build_approved_grant,
        build_plugin_settings_group, current_settings_snapshot, database_backends_requiring_grant,
        normalize_disabled_shelves, normalize_setting_value, title_id_candidates,
        validate_daemon_listen, validate_daemon_listen_against_auth, InviteLinkError,
        PluginGrantOverride,
    };
    use bookclerk_config::{Config, ListenAddrs};

    #[test]
    fn title_id_candidates_dedupes_case_folds() {
        assert_eq!(
            title_id_candidates("B00Test"),
            vec!["B00Test", "b00test", "B00TEST"]
        );
        assert_eq!(title_id_candidates("b00test"), vec!["b00test", "B00TEST"]);
        assert_eq!(title_id_candidates("B00TEST"), vec!["B00TEST", "b00test"]);
        assert!(title_id_candidates("").is_empty());
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
        assert!(allowed_setting_key("sources.audible.region"));
        assert!(allowed_setting_key("sources.libro.enabled_mode"));
        assert!(!allowed_setting_key("sources"));
        assert!(!allowed_setting_key("sources..region"));
        assert!(!allowed_setting_key("sources.audible"));
        assert!(!allowed_setting_key("sources.audible.region.extra"));
        assert!(allowed_setting_key("integrations.audiobookshelf.enabled"));
        assert!(allowed_setting_key("output.s3.bucket"));
        assert!(allowed_setting_key("database.plugin"));
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
            plugin_id: "demo".into(),
            kind: "source".into(),
            network_mode: "outbound".into(),
            domains: BTreeSet::from(["a.example".into(), "b.example".into()]),
            bindings: BTreeSet::from(["config".into(), "secrets".into()]),
            compatibility_flags: BTreeSet::new(),
            cpu_ms: Some(30_000),
            subrequests: Some(50),
            disk_mib: Some(512),
            memory_mib: Some(512),
            cpu_rate_percent: Some(80),
            extra_processes: Some(2),
            approved_at: "2026-01-01T00:00:00Z".into(),
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
                bindings: Some(vec!["config".into(), "secrets".into(), "oauth".into()]),
                compatibility_flags: None,
                cpu_ms: Some(60_000),
                subrequests: None,
                disk_mib: Some(2048),
                memory_mib: Some(1024),
                cpu_cores: Some(0.90),
                extra_processes: Some(8),
            }),
        )
        .expect("widen beyond baseline");
        assert!(widen.domains.contains("evil.example"));
        assert!(widen.bindings.contains("oauth"));
        assert_eq!(widen.cpu_ms, Some(60_000));
        assert_eq!(widen.disk_mib, Some(2048));
        assert_eq!(widen.memory_mib, Some(1024));
        assert_eq!(widen.cpu_rate_percent, Some(90));
        assert_eq!(widen.extra_processes, Some(8));
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
    }

    #[test]
    fn built_in_plugin_groups_include_registered_optional_settings() {
        let cfg = Config::default();

        let audible = build_plugin_settings_group(
            &cfg,
            bookclerk_plugin_host::PluginKind::Source,
            "audible",
            toml::Table::new(),
        );
        assert!(audible
            .settings
            .iter()
            .find(|option| option.key == "sources.audible.bitrate")
            .and_then(|option| option.choices.as_ref())
            .is_some());

        let abs = build_plugin_settings_group(
            &cfg,
            bookclerk_plugin_host::PluginKind::Integration,
            "audiobookshelf",
            toml::Table::new(),
        );
        assert!(abs
            .settings
            .iter()
            .any(|option| option.key == "integrations.audiobookshelf.base_url"));
        assert!(abs
            .settings
            .iter()
            .any(|option| option.key == "integrations.audiobookshelf.api_key"));
        assert!(abs
            .settings
            .iter()
            .any(|option| option.key == "integrations.audiobookshelf.library_id"));

        let s3 = build_plugin_settings_group(
            &cfg,
            bookclerk_plugin_host::PluginKind::Output,
            "s3",
            toml::Table::new(),
        );
        assert!(s3
            .settings
            .iter()
            .any(|option| option.key == "output.s3.endpoint"));
        assert!(s3
            .settings
            .iter()
            .any(|option| option.key == "output.s3.bucket"));

        let d1 = build_plugin_settings_group(
            &cfg,
            bookclerk_plugin_host::PluginKind::Database,
            "d1",
            toml::Table::new(),
        );
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

        let postgres = build_plugin_settings_group(
            &cfg,
            bookclerk_plugin_host::PluginKind::Database,
            "postgres",
            toml::Table::new(),
        );
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
        )
        .expect("disable");
        assert_eq!(cfg.database.plugin, "");

        apply_database_enable_updates(&mut cfg, &[("database.d1.enabled".into(), "true".into())])
            .expect("enable d1");
        assert_eq!(cfg.database.plugin, "d1");

        apply_database_enable_updates(
            &mut cfg,
            &[
                ("database.d1.enabled".into(), "false".into()),
                ("database.postgres.enabled".into(), "true".into()),
            ],
        )
        .expect("switch");
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
api_version = 2
id = "logo_echo"
kind = "integration"
version = "0.1.0"
{logo_line}
runtime = "native"
command = "./guest"
[capabilities.network]
mode = "deny"
"#
            ),
        )
        .unwrap();
        root
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
        });

        let app = Router::new()
            .route("/api/library/scan", post(super::trigger_scan))
            .route("/api/library/acquire", post(super::trigger_acquire))
            .route("/api/jobs", get(super::list_jobs))
            .route("/api/jobs/{id}/cancel", post(super::cancel_job))
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
}
