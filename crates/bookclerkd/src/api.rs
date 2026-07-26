//! HTTP control plane for `bookclerkd` (operator API + static GUI).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use axum::Router;
use bookclerk_acquire::sidecar_key;
use bookclerk_config::Config;
use bookclerk_integrations::{portal_router, IntegrationRegistry, PortalState};
use bookclerk_library::{AcquireStatus, BookRecord, LibraryStore};
use bookclerk_search::{SearchEngine, SearchHit};
use bookclerk_source::ContentSource;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};
use tower_http::normalize_path::NormalizePathLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

use crate::auth::{self, OperatorAuthState};
use crate::jobs::{enqueue_acquire, enqueue_scan};

/// Shared daemon state.
pub struct AppState {
    pub config: Arc<RwLock<Config>>,
    pub library: LibraryStore,
    pub jobs: Arc<RwLock<Vec<JobInfo>>>,
    /// Serialize scan/acquire work so jobs do not thrash the same accounts.
    pub work_lock: Mutex<()>,
    pub integrations: IntegrationRegistry,
    pub sources: Vec<Arc<dyn ContentSource>>,
    pub auth: Option<Arc<OperatorAuthState>>,
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

/// Build the HTTP router (API + optional static UI + Connect portal).
pub fn router(
    state: Arc<AppState>,
    portal_base: String,
    files_dir: PathBuf,
    ui_dist: Option<PathBuf>,
) -> Router {
    let portal_state = PortalState {
        config: state.config.clone(),
        library: state.library.clone(),
        integrations: state.integrations.clone(),
        files_dir,
        sources: state.sources.clone(),
    };

    let protected = Router::new()
        .route("/status", get(status))
        .route("/scan", post(trigger_scan))
        .route("/acquire", post(trigger_acquire))
        .route("/jobs", get(list_jobs))
        .route("/integrations/{id}/scan", post(trigger_integration_scan))
        .route("/api/status", get(status))
        .route("/api/jobs", get(list_jobs))
        .route("/api/library/scan", post(trigger_scan))
        .route("/api/library/acquire", post(trigger_acquire))
        .route("/api/library/books", get(list_books))
        .route("/api/library/books/{uuid}", get(get_book))
        .route("/api/library/books/{uuid}/cover", get(get_book_cover))
        .route("/api/auth/logout", post(auth::logout))
        .route("/api/auth/me", get(auth::me))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_operator_auth,
        ))
        .with_state(state.clone());

    let mut app = Router::new()
        .route("/health", get(health))
        .route("/api/auth/login", post(auth::login))
        .merge(protected)
        .with_state(state);

    if !portal_base.is_empty() {
        app = app.nest(&portal_base, portal_router(portal_state));
    }

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

    // Outermost: normalize `/connect/` → `/connect` before route matching.
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

async fn status(State(state): State<Arc<AppState>>) -> Result<Json<StatusResponse>, StatusCode> {
    let accounts = state
        .library
        .list_accounts()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .len();
    let books = state
        .library
        .list_books(None)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .len();
    let acquired = state
        .library
        .count_by_status(AcquireStatus::Acquired)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let pending = state
        .library
        .count_by_status(AcquireStatus::NotAcquired)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let error = state
        .library
        .count_by_status(AcquireStatus::Error)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let queued = state
        .library
        .count_by_status(AcquireStatus::Queued)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let downloading = state
        .library
        .count_by_status(AcquireStatus::Downloading)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let cfg = state.config.read().await;
    Ok(Json(StatusResponse {
        accounts,
        books,
        acquired,
        pending,
        error,
        in_progress: queued + downloading,
        listen: cfg.daemon.listen.clone(),
        storage_backend: {
            let names = cfg.output.enabled_backend_names();
            if names.is_empty() {
                "none".into()
            } else {
                names.join(",")
            }
        },
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
    Query(query): Query<BooksQuery>,
) -> Result<Json<BooksResponse>, StatusCode> {
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let offset = query.offset.unwrap_or(0);
    let status_filter = query.status.as_deref().and_then(AcquireStatus::parse);

    let mut books = if let Some(q) = query.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let cfg = state.config.read().await;
        let engine = SearchEngine::open(&cfg.paths().search_index_dir)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let hits = engine
            .search(q, 500)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let mut out = Vec::new();
        for hit in hits {
            if let Some(account) = query.account.as_deref() {
                if hit.account_id != account {
                    continue;
                }
            }
            if let Some(book) = book_for_search_hit(&state.library, &hit)
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            {
                out.push(book);
            }
        }
        out
    } else {
        state
            .library
            .list_books(query.account.as_deref())
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    };

    if let Some(status) = status_filter {
        books.retain(|b| b.acquire_status == status);
    }

    // Stable-ish ordering: title then uuid.
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
    state
        .library
        .get_book_by_uuid(&uuid)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

async fn get_book_cover(
    State(state): State<Arc<AppState>>,
    AxumPath(uuid): AxumPath<String>,
) -> Result<Response, StatusCode> {
    let book = state
        .library
        .get_book_by_uuid(&uuid)
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
fn book_for_search_hit(
    library: &LibraryStore,
    hit: &SearchHit,
) -> Result<Option<BookRecord>, bookclerk_library::LibraryError> {
    if !hit.uuid.is_empty() {
        if let Some(book) = library.get_book_by_uuid(&hit.uuid)? {
            return Ok(Some(book));
        }
    }
    for candidate in title_id_candidates(&hit.asin) {
        if let Some(book) = library.get_book(&candidate, &hit.account_id)? {
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
