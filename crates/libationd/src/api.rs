//! Minimal HTTP control plane for `libationd`.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::Json;
use axum::Router;
use libation_config::Config;
use libation_library::{LiberateStatus, LibraryStore};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};
use tower_http::trace::TraceLayer;

use crate::jobs::{enqueue_liberate, enqueue_scan};

/// Shared daemon state.
pub struct AppState {
    pub config: RwLock<Config>,
    pub library: LibraryStore,
    pub jobs: Arc<RwLock<Vec<JobInfo>>>,
    /// Serialize scan/liberate work so jobs do not thrash the same accounts.
    pub work_lock: Mutex<()>,
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
    liberated: i64,
    /// Titles still needing liberate (`not_liberated`).
    pending: i64,
    /// Titles stuck in `error` after a failed liberate.
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
pub struct LiberateRequestBody {
    pub asin: Option<String>,
    pub account: Option<String>,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/scan", post(trigger_scan))
        .route("/liberate", post(trigger_liberate))
        .route("/jobs", get(list_jobs))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
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
    let liberated = state
        .library
        .count_by_status(LiberateStatus::Liberated)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let pending = state
        .library
        .count_by_status(LiberateStatus::NotLiberated)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let error = state
        .library
        .count_by_status(LiberateStatus::Error)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let queued = state
        .library
        .count_by_status(LiberateStatus::Queued)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let downloading = state
        .library
        .count_by_status(LiberateStatus::Downloading)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let cfg = state.config.read().await;
    Ok(Json(StatusResponse {
        accounts,
        books,
        liberated,
        pending,
        error,
        in_progress: queued + downloading,
        listen: cfg.daemon.listen.clone(),
        storage_backend: format!("{:?}", cfg.storage.backend).to_ascii_lowercase(),
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

async fn trigger_liberate(
    State(state): State<Arc<AppState>>,
    body: Option<Json<LiberateRequestBody>>,
) -> Json<ActionResponse> {
    let (asin, account) = body.map(|Json(b)| (b.asin, b.account)).unwrap_or_default();
    let id = enqueue_liberate(state, asin, account).await;
    Json(ActionResponse {
        ok: true,
        message: format!("liberate job {id} accepted"),
        job_id: id,
    })
}

async fn list_jobs(State(state): State<Arc<AppState>>) -> Json<Vec<JobInfo>> {
    Json(state.jobs.read().await.clone())
}
