//! Runtime application of database-backed `[events]` configuration.
//!
//! The dispatcher and pruner read [`bookclerk_config::Config::events`] on each
//! tick. This module publishes committed revisions into that struct. Local
//! delivery-task count is chosen when the event runtime starts and stays at
//! that count until process restart; retention and the in-flight cap follow
//! the latest applied revision.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use bookclerk_library::control_plane::{
    load_events, overlay_events, replace_events, ConfigActor, EventsReplace, EventsSettingsV1,
    CONFIG_RECONCILE_INTERVAL,
};
use serde::Deserialize;
use serde_json::json;

use crate::api::AppState;

/// Operator actor recorded on configuration commits from the daemon.
fn operator_actor() -> ConfigActor {
    ConfigActor::Operator {
        id: "operator".into(),
    }
}

/// Re-reads `core.events` until the process stops.
pub fn spawn_config_reconciler(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(CONFIG_RECONCILE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(err) = reconcile_events(&state).await {
                tracing::warn!(error = %err, "configuration reconcile failed");
            }
        }
    });
}

/// Applies a newer cluster events document onto the live config.
///
/// # Errors
///
/// Returns an error when the document cannot be read. The in-memory config is
/// left unchanged, so a bad revision is not announced as applied.
pub async fn reconcile_events(state: &AppState) -> anyhow::Result<()> {
    let library = state.library_snapshot().await;
    let events = load_events(&library).await?;
    let mut config = state.config.write().await;
    if config.events_revision == Some(events.revision) {
        return Ok(());
    }
    let revision = events.revision;
    overlay_events(&mut config, &events);
    tracing::info!(revision, "applied core.events from the database");
    Ok(())
}

/// JSON body for `PUT /api/config/domains/core.events`.
#[derive(Debug, Deserialize)]
pub struct PutEventsDomainRequest {
    /// Revision the caller read. A mismatch is a conflict.
    pub expected_revision: i64,
    /// Replacement `[events]` document.
    pub retention_days: u64,
    /// Replacement dead-letter retention.
    pub dead_letter_retention_days: u64,
    /// Replacement concurrency cap.
    pub concurrency: u32,
}

/// `GET /api/config/domains/core.events`.
pub async fn get_events_domain(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, Response> {
    let library = state.library_snapshot().await;
    let doc = load_events(&library).await.map_err(events_error)?;
    Ok(Json(events_json(&doc)))
}

/// `PUT /api/config/domains/core.events`.
pub async fn put_events_domain(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PutEventsDomainRequest>,
) -> Result<Json<serde_json::Value>, Response> {
    let replacement = EventsSettingsV1 {
        retention_days: body.retention_days,
        dead_letter_retention_days: body.dead_letter_retention_days,
        concurrency: body.concurrency,
    };
    let library = state.library_snapshot().await;
    let outcome = replace_events(
        &library,
        &operator_actor(),
        body.expected_revision,
        &replacement,
        &uuid::Uuid::new_v4().to_string(),
    )
    .await
    .map_err(events_error)?;
    match outcome {
        EventsReplace::Applied(doc) => {
            publish_events(state.as_ref(), &doc).await;
            Ok(Json(events_json(&doc)))
        }
        EventsReplace::Replayed { revision } => Ok(Json(json!({
            "namespace": "core.events",
            "replayed": true,
            "revision": revision,
        }))),
        EventsReplace::Conflict { current_revision } => Err(conflict(current_revision)),
    }
}

/// Applies `events.*` settings keys through compare-and-swap.
///
/// # Errors
///
/// Returns 400 for invalid values and 409 when `expected` revision lost.
pub async fn commit_events_settings(
    state: &AppState,
    updates: &[(String, String)],
) -> Result<(), Response> {
    if updates.is_empty() {
        return Ok(());
    }
    let library = state.library_snapshot().await;
    let current = load_events(&library).await.map_err(events_error)?;
    let mut body = current.body.clone();
    for (key, value) in updates {
        apply_events_key(&mut body, key, value).map_err(|err| {
            tracing::warn!(error = %err, "rejected events settings update");
            StatusCode::BAD_REQUEST.into_response()
        })?;
    }
    if body == current.body {
        return Ok(());
    }
    let outcome = replace_events(
        &library,
        &operator_actor(),
        current.revision,
        &body,
        &uuid::Uuid::new_v4().to_string(),
    )
    .await
    .map_err(events_error)?;
    match outcome {
        EventsReplace::Applied(doc) => {
            publish_events(state, &doc).await;
            Ok(())
        }
        EventsReplace::Replayed { .. } => Ok(()),
        EventsReplace::Conflict { current_revision } => Err(conflict(current_revision)),
    }
}

/// Copies a committed events document into the live daemon config.
async fn publish_events(
    state: &AppState,
    doc: &bookclerk_library::control_plane::ConfigurationDocument<EventsSettingsV1>,
) {
    let mut config = state.config.write().await;
    overlay_events(&mut config, doc);
}

/// Applies one `events.*` settings key onto a typed document.
fn apply_events_key(body: &mut EventsSettingsV1, key: &str, value: &str) -> Result<(), String> {
    match key {
        "events.retention_days" => {
            body.retention_days = value
                .parse()
                .map_err(|_| "events.retention_days must be an integer".to_string())?;
        }
        "events.dead_letter_retention_days" => {
            body.dead_letter_retention_days = value
                .parse()
                .map_err(|_| "events.dead_letter_retention_days must be an integer".to_string())?;
        }
        "events.concurrency" => {
            body.concurrency = value
                .parse()
                .map_err(|_| "events.concurrency must be an integer".to_string())?;
        }
        other => return Err(format!("unsupported events setting key: {other}")),
    }
    body.validate().map_err(|err| err.to_string())
}

/// JSON body for the events domain API.
fn events_json(
    doc: &bookclerk_library::control_plane::ConfigurationDocument<EventsSettingsV1>,
) -> serde_json::Value {
    json!({
        "namespace": doc.namespace,
        "scope_type": doc.scope_type,
        "scope_id": doc.scope_id,
        "schema_version": doc.schema_version,
        "revision": doc.revision,
        "updated_at": doc.updated_at,
        "updated_by": doc.updated_by,
        "retention_days": doc.body.retention_days,
        "dead_letter_retention_days": doc.body.dead_letter_retention_days,
        "concurrency": doc.body.concurrency,
    })
}

/// 409 body carrying the revision that won.
fn conflict(current_revision: i64) -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({
            "error": "revision_conflict",
            "current_revision": current_revision,
        })),
    )
        .into_response()
}

/// Maps a library configuration error onto an HTTP status.
fn events_error(err: bookclerk_library::LibraryError) -> Response {
    let message = err.to_string();
    let status = if message.contains("unauthorized configuration write") {
        StatusCode::FORBIDDEN
    } else if message.contains("unsupported configuration schema")
        || message.contains("invalid configuration")
    {
        StatusCode::BAD_REQUEST
    } else if message.contains("not initialized") || message.contains("not found") {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (
        status,
        Json(json!({
            "error": message,
        })),
    )
        .into_response()
}
