//! HTTP control plane for `bookclerkd` (operator API + static GUI).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::Json;
use axum::Router;
use bookclerk_acquire::sidecar_key;
use bookclerk_config::Config;
use bookclerk_integrations::{portal_spa_router, IntegrationRegistry, PortalState};
use bookclerk_library::{
    configure_master_key_with, AcquireStatus, BookRecord, LibraryStore, NewTitleRequest,
    RequestStatus, TitleRequestRecord,
};
use bookclerk_plugin::{DatabaseRegistry, DestinationRegistry};
use bookclerk_search::{SearchEngine, SearchHit};
use bookclerk_source::SourceRegistry;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify, RwLock};
use tower_http::normalize_path::NormalizePathLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

use crate::auth::{self, OperatorAuthState};
use crate::jobs::{enqueue_acquire, enqueue_scan};

/// Shared daemon state.
pub struct AppState {
    pub config: Arc<RwLock<Config>>,
    pub library: Arc<RwLock<LibraryStore>>,
    pub database_registry: Arc<RwLock<DatabaseRegistry>>,
    pub jobs: Arc<RwLock<Vec<JobInfo>>>,
    /// Serialize scan/acquire work so jobs do not thrash the same accounts.
    ///
    /// This is about store rate limits and the shared `StorageIndex`, not about
    /// CPU: codec concurrency is bounded by the media worker pool instead.
    /// Letting independent books acquire in parallel needs a concurrency-safe
    /// storage index first.
    pub work_lock: Mutex<()>,
    pub integrations: IntegrationRegistry,
    pub sources: SourceRegistry,
    pub destinations: Arc<RwLock<DestinationRegistry>>,
    pub auth: Option<Arc<OperatorAuthState>>,
    /// Wakes the HTTP server to rebind when `daemon.listen` changes on config reload.
    pub listen_reload: Arc<Notify>,
}

impl AppState {
    /// Cheap clone of the live library handle; drop the read lock before awaiting DB I/O.
    pub async fn library_snapshot(&self) -> LibraryStore {
        self.library.read().await.clone()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobInfo {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub detail: Option<String>,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    accounts: usize,
    books: usize,
    acquired: i64,
    /// Titles still needing acquire (`not_acquired`).
    pending: i64,
    /// Titles stuck in `error` after a failed acquire.
    error: i64,
    /// Titles currently `queued` or `downloading`.
    in_progress: i64,
    listen: String,
    storage_backend: String,
}

#[derive(Debug, Serialize)]
struct ActionResponse {
    ok: bool,
    message: String,
    job_id: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct ScanRequest {
    pub account: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct AcquireRequestBody {
    pub asin: Option<String>,
    pub uuid: Option<String>,
    pub isbn: Option<String>,
    pub product_id: Option<String>,
    pub account: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct IntegrationScanRequest {
    pub force: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct DatabaseMigrateRequest {
    /// Source plugin id. Defaults to the active `[database].plugin` before reload.
    from: Option<String>,
    to: String,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    force: bool,
    /// Update `[database].plugin` in config.toml after a successful copy.
    #[serde(default)]
    apply: bool,
}

#[derive(Debug, Deserialize, Default)]
struct BooksQuery {
    account: Option<String>,
    status: Option<String>,
    q: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

#[derive(Debug, Serialize)]
struct BooksResponse {
    books: Vec<BookRecord>,
    total: usize,
    limit: usize,
    offset: usize,
}

/// Build the HTTP router (API + optional static UI + SPA portal APIs).
pub fn router(state: Arc<AppState>, ui_dist: Option<PathBuf>) -> Router {
    let portal_state = PortalState {
        config: state.config.clone(),
        library: state.library.clone(),
        integrations: state.integrations.clone(),
        sources: state.sources.all(),
    };

    let operator_only = Router::new()
        .route("/status", get(status))
        .route("/scan", post(trigger_scan))
        .route("/acquire", post(trigger_acquire))
        .route("/jobs", get(list_jobs))
        .route("/integrations/{id}/scan", post(trigger_integration_scan))
        .route("/api/status", get(status))
        .route("/api/config/reload", post(reload_config))
        .route("/api/database/migrate", post(migrate_database))
        .route("/api/jobs", get(list_jobs))
        .route("/api/library/scan", post(trigger_scan))
        .route("/api/library/acquire", post(trigger_acquire))
        .route("/api/discover/sync-listening", post(sync_listening))
        .layer(middleware::from_fn_with_state(
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
        .route("/api/wishlist", get(list_wishlist).post(create_wishlist))
        .route("/api/wishlist/{uuid}", delete(delete_wishlist))
        .route("/api/request-queue", get(list_request_queue))
        .route("/api/auth/logout", post(auth::logout))
        .route("/api/auth/me", get(auth::me))
        .route(
            "/api/preferences",
            get(get_preferences).patch(patch_preferences),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_operator_or_portal_auth,
        ))
        .with_state(state.clone());

    let mut app = Router::new()
        .route("/health", get(health))
        .route("/api/auth/login", post(auth::login))
        .merge(operator_only)
        .merge(shared)
        .with_state(state);

    // SPA Accounts / claim APIs (Path=/ portal session cookie).
    app = app.nest("/api/portal", portal_spa_router(portal_state));

    if let Some(dist) = ui_dist {
        if dist.is_dir() {
            let index = dist.join("index.html");
            let service = ServeDir::new(dist.clone()).not_found_service(ServeFile::new(index));
            tracing::info!(path = %dist.display(), "serving GUI static assets");
            app = app.fallback_service(service);
        } else {
            tracing::warn!(
                path = %dist.display(),
                "GUI dist path missing; static UI not served (build ui/ or set BOOKCLERK_UI_DIST)"
            );
        }
    }

    // Outermost: normalize trailing slashes before route matching.
    app.layer(NormalizePathLayer::trim_trailing_slash())
        .layer(TraceLayer::new_for_http())
}

/// Resolve the Vite build output directory for the GUI.
#[must_use]
pub fn resolve_ui_dist() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("BOOKCLERK_UI_DIST") {
        let path = PathBuf::from(v.trim());
        if path.is_dir() {
            return Some(path);
        }
    }
    let candidates = [
        PathBuf::from("ui/dist"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/dist"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ui-dist"),
    ];
    candidates.into_iter().find(|p| p.is_dir())
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

/// Re-open the library connection for the active `[database].plugin` and refresh destinations.
pub async fn reload_library_store(state: &AppState, config: &Config) -> anyhow::Result<()> {
    let registry = bookclerk_plugin::load_external_database(config).await?;
    let library = bookclerk_plugin::open_library_store(config, &registry).await?;
    let destinations =
        bookclerk_plugin::load_external_destinations(config, Some(library.db())).await?;
    *state.database_registry.write().await = registry;
    *state.library.write().await = library;
    *state.destinations.write().await = destinations;
    tracing::info!(
        plugin = %config.database.plugin,
        "reloaded library database connection"
    );
    Ok(())
}

/// Reload `config.toml` from disk and re-apply master-key wrap (BCK1→BCK2).
pub async fn reload_daemon_config(state: &AppState) -> anyhow::Result<String> {
    let (files_dir, config_path, old_listen) = {
        let cfg = state.config.read().await;
        (
            cfg.paths().files_dir.clone(),
            cfg.paths().config_file.clone(),
            cfg.daemon.listen.clone(),
        )
    };
    let new_cfg = Config::load(Some(files_dir.clone()), Some(config_path.clone()))?;
    validate_daemon_listen(&new_cfg)?;
    configure_master_key_with(&files_dir, new_cfg.auth_password().as_deref())?;
    new_cfg.warn_unsupported_options();
    let old_db_plugin = {
        let cfg = state.config.read().await;
        cfg.database.plugin.clone()
    };
    // A changed [media] swaps in a new pool for subsequent jobs and lets the
    // old one drain; see `init_pool_from_config`.
    bookclerk_media::init_pool_from_config(&new_cfg.media);
    let db_plugin_changed = !old_db_plugin.eq_ignore_ascii_case(&new_cfg.database.plugin);
    if db_plugin_changed {
        reload_library_store(state, &new_cfg).await?;
    } else {
        let library = state.library_snapshot().await;
        let destinations =
            bookclerk_plugin::load_external_destinations(&new_cfg, Some(library.db())).await?;
        *state.destinations.write().await = destinations;
    }
    let listen_changed = old_listen != new_cfg.daemon.listen;
    let wrapped = new_cfg.auth_password().is_some();
    *state.config.write().await = new_cfg.clone();
    let mut detail = format!(
        "reloaded {} (master.key wrap={wrapped})",
        config_path.display()
    );
    if db_plugin_changed {
        detail.push_str(&format!(
            "; switched database plugin `{old_db_plugin}` → `{}`",
            new_cfg.database.plugin
        ));
    }
    if listen_changed {
        detail.push_str(&format!(
            "; rebinding HTTP listener `{old_listen}` → `{}`",
            new_cfg.daemon.listen
        ));
        state.listen_reload.notify_waiters();
    }
    Ok(detail)
}

/// Parse `daemon.listen` and reject unsafe auth/listen combinations.
pub fn validate_daemon_listen(config: &Config) -> anyhow::Result<()> {
    let addr = config.daemon.listen.parse::<SocketAddr>().map_err(|err| {
        anyhow::anyhow!("invalid daemon.listen '{}': {err}", config.daemon.listen)
    })?;
    if !config.daemon.auth.enabled && !addr.ip().is_loopback() {
        anyhow::bail!(
            "daemon.auth.enabled=false is unsafe when listen is not loopback ({})",
            config.daemon.listen
        );
    }
    Ok(())
}

async fn reload_config(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ActionResponse>, StatusCode> {
    match reload_daemon_config(&state).await {
        Ok(message) => Ok(Json(ActionResponse {
            ok: true,
            message,
            job_id: String::new(),
        })),
        Err(err) => {
            tracing::error!(error = %err, "config reload failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

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
    match bookclerk_plugin::migrate_database_plugin(
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
                let mut new_cfg = cfg.clone();
                new_cfg.database.plugin = to_plugin.clone();
                let path = new_cfg.paths().config_file.clone();
                new_cfg
                    .write_toml_file(&path)
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                reload_library_store(&state, &new_cfg)
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                *state.config.write().await = new_cfg;
                message.push_str(&format!(
                    "; updated [database].plugin, wrote {}, and reloaded library connection",
                    path.display()
                ));
            }
            Ok(Json(ActionResponse {
                ok: true,
                message,
                job_id: String::new(),
            }))
        }
        Err(err) => {
            tracing::error!(error = %err, "database migrate failed");
            Err(StatusCode::BAD_REQUEST)
        }
    }
}

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
            cfg.daemon.listen.clone(),
            if names.is_empty() {
                "none".into()
            } else {
                names.join(",")
            },
        )
    };
    Ok(Json(StatusResponse {
        accounts,
        books,
        acquired,
        pending,
        error,
        in_progress,
        listen,
        storage_backend,
    }))
}

async fn trigger_scan(
    State(state): State<Arc<AppState>>,
    body: Option<Json<ScanRequest>>,
) -> Json<ActionResponse> {
    let account = body.and_then(|Json(b)| b.account);
    let id = enqueue_scan(state, account).await;
    Json(ActionResponse {
        ok: true,
        message: format!("scan job {id} accepted"),
        job_id: id,
    })
}

async fn trigger_acquire(
    State(state): State<Arc<AppState>>,
    body: Option<Json<AcquireRequestBody>>,
) -> Json<ActionResponse> {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let title_filter = body.uuid.or(body.asin).or(body.isbn).or(body.product_id);
    let id = enqueue_acquire(state, title_filter, body.account).await;
    Json(ActionResponse {
        ok: true,
        message: format!("acquire job {id} accepted"),
        job_id: id,
    })
}

async fn list_jobs(State(state): State<Arc<AppState>>) -> Json<Vec<JobInfo>> {
    Json(state.jobs.read().await.clone())
}

async fn trigger_integration_scan(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    body: Option<Json<IntegrationScanRequest>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let force = body.and_then(|Json(b)| b.force).unwrap_or(false);
    let Some(integration) = state.integrations.get(&id) else {
        return Err(StatusCode::NOT_FOUND);
    };
    if !integration.supports_library_scan() {
        return Err(StatusCode::BAD_REQUEST);
    }
    integration
        .scan_library(force)
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    Ok(Json(serde_json::json!({ "ok": true, "integration": id })))
}

async fn list_books(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<BooksQuery>,
) -> Result<Json<BooksResponse>, StatusCode> {
    let library = state.library_snapshot().await;
    let limit = query.limit.unwrap_or(40).clamp(1, 500);
    let offset = query.offset.unwrap_or(0);
    let status_filter = query.status.as_deref().and_then(AcquireStatus::parse);

    // Portal users only see books from accounts they linked (contributed).
    let portal_accounts: Option<std::collections::HashSet<String>> =
        if let Some(identity) = auth::caller_portal_identity(&state, &headers).await {
            let links = library
                .list_account_links(identity.id)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            Some(links.into_iter().map(|l| l.account_id).collect())
        } else {
            None
        };

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
            if let Some(allowed) = portal_accounts.as_ref() {
                if !allowed.contains(&hit.account_id) {
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
        if let Some(allowed) = portal_accounts.as_ref() {
            if !allowed.contains(account) {
                Vec::new()
            } else {
                library
                    .list_books(Some(account))
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            }
        } else {
            library
                .list_books(Some(account))
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        }
    } else if let Some(allowed) = portal_accounts.as_ref() {
        let mut out = Vec::new();
        for account_id in allowed {
            out.extend(
                library
                    .list_books(Some(account_id))
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
            );
        }
        out
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
struct RecommendQuery {
    limit: Option<usize>,
    user: Option<String>,
    #[serde(default)]
    no_purchase_hints: Option<bool>,
    /// When true, ignore listening_progress (owned-library taste only).
    #[serde(default)]
    no_listening: Option<bool>,
    /// Comma-separated integration ids; empty = all listening providers.
    #[serde(default)]
    listening_providers: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateRequestBody {
    title: String,
    authors: Option<String>,
    asin: Option<String>,
    isbn: Option<String>,
    notes: Option<String>,
    /// Optional known storefront editions (for work_key / metadata only).
    #[serde(default)]
    store_editions: Vec<bookclerk_discover::StoreEdition>,
    work_key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CatalogSearchQuery {
    q: Option<String>,
    limit: Option<usize>,
    region: Option<String>,
}

async fn discover_recommendations(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<RecommendQuery>,
) -> Result<Json<bookclerk_discover::DiscoverFeed>, (StatusCode, String)> {
    let cfg = state.config.read().await.clone();
    let library = state.library_snapshot().await;
    let _ = bookclerk_discover::rebuild_works_from_library(&library)
        .await
        .map_err(internal_err)?;

    let mut embedder = bookclerk_discover::open_embedder(
        &cfg.paths().models_dir,
        cfg.discovery.embed_intra_threads,
        cfg.discovery.embeddings_enabled,
    )
    .map_err(internal_err)?;
    let model_id = embedder.model_id().to_string();
    let _ = bookclerk_discover::embed_dirty_works(&library, embedder.as_mut()).await;

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

    let (subject_key, identity_id) = auth::prefs_subject_for_caller(&state, &headers).await;
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
        fetch_storefront_candidates: cfg.discovery.storefront_candidates,
        storefront_seed_limit: cfg.discovery.storefront_seed_limit,
        storefront_max_remote_calls: cfg.discovery.storefront_max_remote_calls,
        exclude_graphicaudio_series_sets: cfg.discovery.exclude_graphicaudio_series_sets,
        disabled_shelves,
        models_dir: Some(cfg.paths().models_dir.clone()),
        embed_intra_threads: cfg.discovery.embed_intra_threads,
        embeddings_enabled: cfg.discovery.embeddings_enabled,
    };
    let feed = bookclerk_discover::recommend_feed(&library, &state.sources, &opts)
        .await
        .map_err(internal_err)?;
    Ok(Json(feed))
}

async fn discover_purchase_hints(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(mut body): Json<bookclerk_discover::PurchaseHintsQuery>,
) -> Result<Json<bookclerk_discover::PurchaseHintsResponse>, (StatusCode, String)> {
    validate_purchase_hints_query(&body)?;
    // Always derive linked stores server-side (ignore client-supplied overrides).
    body.preferred_sources = preferred_sources_for_caller(&state, &headers).await;
    let response = bookclerk_discover::resolve_purchase_hints(&state.sources, &body)
        .await
        .map_err(internal_err)?;
    Ok(Json(response))
}

#[derive(Debug, serde::Deserialize)]
struct PurchaseHintsBatchBody {
    #[serde(default)]
    queries: Vec<bookclerk_discover::PurchaseHintsQuery>,
}

#[derive(Debug, serde::Serialize)]
struct PurchaseHintsBatchResponse {
    results: Vec<bookclerk_discover::PurchaseHintsResponse>,
}

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
    let resolved =
        bookclerk_discover::resolve_purchase_hints_batch(&state.sources, &queries, 4).await;
    let mut results = Vec::with_capacity(resolved.len());
    for item in resolved {
        results.push(item.map_err(internal_err)?);
    }
    Ok(Json(PurchaseHintsBatchResponse { results }))
}

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

async fn discover_catalog_search(
    State(state): State<Arc<AppState>>,
    Query(q): Query<CatalogSearchQuery>,
) -> Result<Json<Vec<bookclerk_discover::CatalogSearchHit>>, (StatusCode, String)> {
    let query = q.q.unwrap_or_default();
    if query.trim().len() < 2 {
        return Ok(Json(Vec::new()));
    }
    let region = q.region.unwrap_or_else(|| String::from("us"));
    let limit = q.limit.unwrap_or(12).clamp(1, 24);
    let hits = bookclerk_discover::catalog_search(&state.sources, &query, &region, limit)
        .await
        .map_err(internal_err)?;
    Ok(Json(hits))
}

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

async fn create_wishlist(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<CreateRequestBody>,
) -> Result<Json<TitleRequestRecord>, (StatusCode, String)> {
    create_request_inner(&state, &headers, body).await
}

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

async fn list_request_queue(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<bookclerk_discover::RankedQueueEntry>>, (StatusCode, String)> {
    let cfg = state.config.read().await.clone();
    let library = state.library_snapshot().await;

    let mut embedder = bookclerk_discover::open_embedder(
        &cfg.paths().models_dir,
        cfg.discovery.embed_intra_threads,
        cfg.discovery.embeddings_enabled,
    )
    .map_err(internal_err)?;
    let model_id = embedder.model_id().to_string();
    let _ = bookclerk_discover::embed_dirty_works(&library, embedder.as_mut()).await;

    // Shared queue: overall / operator taste only (no portal personalization).
    let opts = bookclerk_discover::RecommendOptions {
        limit: cfg.discovery.recommend_limit.max(24),
        embedding_model: model_id,
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
        models_dir: Some(cfg.paths().models_dir.clone()),
        embed_intra_threads: cfg.discovery.embed_intra_threads,
        embeddings_enabled: cfg.discovery.embeddings_enabled,
    };
    let rows = bookclerk_discover::rank_global_request_queue(&library, &state.sources, &opts)
        .await
        .map_err(internal_err)?;
    Ok(Json(rows))
}

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
    let row = library
        .create_title_request(&NewTitleRequest {
            uuid: None,
            identity_id,
            title: body.title,
            authors: body.authors,
            asin: body.asin,
            isbn: body.isbn,
            notes: body.notes,
            status: RequestStatus::Open,
            work_key,
            work_id: None,
            resolved_book_uuid: None,
        })
        .await
        .map_err(internal_err)?;
    Ok(Json(row))
}

async fn sync_listening(
    State(state): State<Arc<AppState>>,
) -> Result<Json<bookclerk_integrations::SyncListeningSummary>, (StatusCode, String)> {
    let library = state.library_snapshot().await;
    let summary = state
        .integrations
        .sync_listening_progress_all(&library)
        .await;
    if summary.by_provider.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "no listening-capable integrations are enabled".into(),
        ));
    }
    Ok(Json(summary))
}

#[derive(Debug, Serialize)]
struct PreferencesResponse {
    default_view: String,
    disabled_shelves: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PatchPreferencesBody {
    default_view: Option<String>,
    disabled_shelves: Option<Vec<String>>,
}

async fn get_preferences(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<PreferencesResponse>, (StatusCode, String)> {
    let (subject_key, identity_id) = auth::prefs_subject_for_caller(&state, &headers).await;
    let library = state.library_snapshot().await;
    let prefs = library
        .get_user_preferences_or_default(&subject_key, identity_id)
        .await
        .map_err(internal_err)?;
    Ok(Json(PreferencesResponse {
        default_view: auth::normalize_default_view(&prefs.default_view),
        disabled_shelves: prefs.disabled_shelves,
    }))
}

async fn patch_preferences(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<PatchPreferencesBody>,
) -> Result<Json<PreferencesResponse>, (StatusCode, String)> {
    let (subject_key, identity_id) = auth::prefs_subject_for_caller(&state, &headers).await;
    let library = state.library_snapshot().await;
    let current = library
        .get_user_preferences_or_default(&subject_key, identity_id)
        .await
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

    let saved = library
        .upsert_user_preferences(&subject_key, identity_id, &default_view, &disabled_shelves)
        .await
        .map_err(internal_err)?;

    Ok(Json(PreferencesResponse {
        default_view: auth::normalize_default_view(&saved.default_view),
        disabled_shelves: saved.disabled_shelves,
    }))
}

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

fn internal_err(err: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

#[cfg(test)]
mod tests {
    use super::title_id_candidates;

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
}
