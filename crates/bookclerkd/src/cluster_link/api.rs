//! Operator API for host consent (H-4 design, step 8):
//!
//! - `GET /api/config/cluster-consents`: the owner clusters this install
//!   is enrolled in, what each asks to run here, and how this host stands
//!   on it (no key material: links carry ids, origin and state only);
//! - `POST /api/config/cluster-consents/approve`: approve one request,
//!   optionally narrowed (denied domains, bindings, or all network);
//! - `POST /api/config/cluster-consents/revoke`: withdraw an approval.
//!
//! - `POST /api/config/clusters/{cluster_id}/scan`: ask the owner cluster
//!   to queue a scan for this node's owner (H-5);
//! - `POST /api/config/clusters/{cluster_id}/jobs`: the owner's state of
//!   jobs this node may see.
//!
//! All of them are operator-only: owner plugins run under this install's
//! jail with this install's network, so only its operator consents. A
//! change wakes every link supervisor in this process
//! ([`super::consent::notify_consent_changed`]).

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use bookclerk_library::control_plane::{
    list_cluster_links, list_remote_plugin_consents, revoke_remote_plugin, ClusterLink,
    RemotePluginConsent,
};
use bookclerk_plugin_host::{approve_remote_request, RemoteNarrowing};
use serde::Deserialize;
use serde_json::json;

use super::consent::{consent_state, notify_consent_changed};
use crate::api::AppState;
use crate::config_authority::events_error;

/// Audit identity of approvals made through the API.
const API_APPROVER: &str = "operator";

/// JSON for one consent row.
pub(crate) fn consent_json(row: &RemotePluginConsent) -> serde_json::Value {
    let parse = |raw: Option<&str>| {
        raw.and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .unwrap_or(serde_json::Value::Null)
    };
    json!({
        "cluster_id": row.owner_cluster_id,
        "plugin_key": row.plugin_key,
        "requested_version": row.requested_version,
        "requested_grant": parse(Some(&row.requested_grant_json)),
        "approved_version": row.version,
        "grant": parse(row.grant_json.as_deref()),
        "approved_at": row.approved_at,
        "approved_by": row.approved_by,
        "updated_at": row.updated_at,
        "state": consent_state(row).as_str(),
    })
}

/// JSON for one cluster link: ids, origin and state, never key material.
pub(crate) fn link_json(link: &ClusterLink) -> serde_json::Value {
    json!({
        "cluster_id": link.owner_cluster_id,
        "owner_origin": link.owner_origin,
        "node_id": link.remote_node_id,
        "pinned": link.spki_pin.is_some(),
        "state": link.state,
        "enrolled_at": link.enrolled_at,
    })
}

/// `GET /api/config/cluster-consents`.
pub async fn get_cluster_consents(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, Response> {
    let library = state.library_snapshot().await;
    let links = list_cluster_links(&library).await.map_err(events_error)?;
    let rows = list_remote_plugin_consents(&library, None)
        .await
        .map_err(events_error)?;
    Ok(Json(json!({
        "clusters": links.iter().map(link_json).collect::<Vec<_>>(),
        "consents": rows.iter().map(consent_json).collect::<Vec<_>>(),
    })))
}

/// `POST /api/config/cluster-consents/approve` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApproveBody {
    /// Owner cluster id.
    cluster_id: String,
    /// Canonical plugin key.
    plugin_key: String,
    /// The requested version the operator reviewed (refused when the
    /// recorded request is another one).
    #[serde(default)]
    version: Option<String>,
    /// Domains to take out of the request.
    #[serde(default)]
    deny_domains: BTreeSet<String>,
    /// Bindings to take out of the request.
    #[serde(default)]
    deny_bindings: BTreeSet<String>,
    /// Deny all network access.
    #[serde(default)]
    deny_network: bool,
}

/// `POST /api/config/cluster-consents/approve`.
pub async fn post_cluster_consent_approve(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ApproveBody>,
) -> Result<Json<serde_json::Value>, Response> {
    let library = state.library_snapshot().await;
    let row = approve_remote_request(
        &library,
        body.cluster_id.trim(),
        body.plugin_key.trim(),
        body.version.as_deref().map(str::trim),
        &RemoteNarrowing {
            deny_domains: body.deny_domains,
            deny_bindings: body.deny_bindings,
            deny_network: body.deny_network,
        },
        API_APPROVER,
    )
    .await
    .map_err(|err| {
        (
            StatusCode::CONFLICT,
            Json(json!({ "error": "approval_refused", "detail": err.to_string() })),
        )
            .into_response()
    })?;
    tracing::info!(
        cluster_id = %row.owner_cluster_id,
        plugin_key = %row.plugin_key,
        "owner plugin approved by this host's operator"
    );
    notify_consent_changed();
    Ok(Json(json!({ "consent": consent_json(&row) })))
}

/// `POST /api/config/cluster-consents/revoke` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeBody {
    /// Owner cluster id.
    cluster_id: String,
    /// Canonical plugin key.
    plugin_key: String,
}

/// `POST /api/config/cluster-consents/revoke`.
pub async fn post_cluster_consent_revoke(
    State(state): State<Arc<AppState>>,
    Json(body): Json<RevokeBody>,
) -> Result<Json<serde_json::Value>, Response> {
    let library = state.library_snapshot().await;
    let row = revoke_remote_plugin(&library, body.cluster_id.trim(), body.plugin_key.trim())
        .await
        .map_err(events_error)?
        .ok_or_else(|| {
            (StatusCode::NOT_FOUND, Json(json!({ "error": "not_found" }))).into_response()
        })?;
    tracing::info!(
        cluster_id = %row.owner_cluster_id,
        plugin_key = %row.plugin_key,
        "owner plugin consent revoked by this host's operator"
    );
    notify_consent_changed();
    Ok(Json(json!({ "consent": consent_json(&row) })))
}

/// `POST /api/config/clusters/{cluster_id}/scan` body.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterScanBody {
    /// Scan only this store account of the owner cluster.
    #[serde(default)]
    account: Option<String>,
    /// Pin every planned job to this owner plugin instance.
    #[serde(default)]
    plugin_instance_id: Option<String>,
}

/// `POST /api/config/clusters/{cluster_id}/jobs` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterJobsBody {
    /// Job ids on the owner cluster.
    job_ids: Vec<String>,
}

/// The live client of `cluster_id`'s link, or 503 `not_connected`.
fn connected_link(cluster_id: &str) -> Result<bookclerk_node_client::HostClientHandle, Response> {
    super::link_handle(cluster_id.trim()).ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "not_connected",
                "detail": "this daemon has no running link to that owner cluster",
            })),
        )
            .into_response()
    })
}

/// Maps an owner's `error` frame (or no answer) onto an HTTP refusal.
fn owner_refusal(answer: Option<bookclerk_node_proto::Frame>) -> Response {
    match answer.map(|frame| frame.body) {
        Some(bookclerk_node_proto::Body::Error { code, message }) => {
            let status = match code.as_str() {
                "forbidden" => StatusCode::FORBIDDEN,
                "invalid" => StatusCode::BAD_REQUEST,
                _ => StatusCode::BAD_GATEWAY,
            };
            (status, Json(json!({ "error": code, "detail": message }))).into_response()
        }
        Some(_) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "unexpected_answer" })),
        )
            .into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "owner_unavailable" })),
        )
            .into_response(),
    }
}

/// `POST /api/config/clusters/{cluster_id}/scan`: asks the owner cluster
/// to queue a scan for this node's owner (H-5). The owner's planner decides
/// what that user may scan; the answer lists the queued job ids.
pub async fn post_cluster_scan(
    axum::extract::Path(cluster_id): axum::extract::Path<String>,
    body: Option<Json<ClusterScanBody>>,
) -> Result<Json<serde_json::Value>, Response> {
    let body = body.map(|Json(body)| body).unwrap_or_default();
    let handle = connected_link(&cluster_id)?;
    let answer = handle
        .request(bookclerk_node_proto::Body::ScanRequest {
            account: body.account.filter(|a| !a.trim().is_empty()),
            plugin_instance_id: body.plugin_instance_id.filter(|i| !i.trim().is_empty()),
        })
        .await;
    match answer {
        Some(bookclerk_node_proto::Frame {
            body:
                bookclerk_node_proto::Body::ScanPlanned {
                    created,
                    duplicates,
                    queue_full,
                },
            ..
        }) => {
            tracing::info!(
                cluster_id = %handle.cluster_id(),
                created = created.len(),
                duplicates = duplicates.len(),
                queue_full,
                "the owner cluster queued a scan for this host"
            );
            Ok(Json(json!({
                "cluster_id": handle.cluster_id(),
                "created": created,
                "duplicates": duplicates,
                "queue_full": queue_full,
            })))
        }
        other => Err(owner_refusal(other)),
    }
}

/// `POST /api/config/clusters/{cluster_id}/jobs`: the owner's view of jobs
/// this node may see (those it would be trusted to claim).
pub async fn post_cluster_jobs(
    axum::extract::Path(cluster_id): axum::extract::Path<String>,
    Json(body): Json<ClusterJobsBody>,
) -> Result<Json<serde_json::Value>, Response> {
    if body.job_ids.len() > bookclerk_node_proto::MAX_STATUS_JOBS {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "too_many_jobs" })),
        )
            .into_response());
    }
    let handle = connected_link(&cluster_id)?;
    match handle
        .request(bookclerk_node_proto::Body::JobStatus {
            job_ids: body.job_ids,
        })
        .await
    {
        Some(bookclerk_node_proto::Frame {
            body: bookclerk_node_proto::Body::JobStatuses { jobs },
            ..
        }) => Ok(Json(json!({ "jobs": jobs }))),
        other => Err(owner_refusal(other)),
    }
}

#[cfg(test)]
#[path = "api_tests.rs"]
mod tests;
