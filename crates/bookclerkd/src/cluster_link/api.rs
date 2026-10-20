//! Operator API for host consent (H-4 design, step 8):
//!
//! - `GET /api/config/cluster-consents`: the owner clusters this install
//!   is enrolled in (no key material: links carry ids, origin and state
//!   only), what each asks to run here (version, grant, and the archive URL
//!   and digest this host would fetch), and how this host stands on it;
//! - `POST /api/config/cluster-consents/approve`: approve one request by
//!   the `request_digest` the listing shows (refused with `409` when the
//!   recorded request is no longer that one, `404` when none is recorded,
//!   `400` for a narrowing it does not ask for, `503` on a store fault),
//!   optionally narrowed (denied domains, bindings, or all network);
//! - `POST /api/config/cluster-consents/revoke`: withdraw an approval (a
//!   revoke holds: a platform plugin is not approved automatically again).
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
    list_cluster_links, list_remote_plugin_consents, load_cluster_link, revoke_remote_plugin,
    ClusterLink, RemoteApprover, RemotePluginConsent,
};
use bookclerk_plugin_host::{
    approve_remote_request, remote_consent_view, RemoteApprovalError, RemoteNarrowing,
};
use serde::Deserialize;
use serde_json::json;

use super::consent::notify_consent_changed;
use crate::api::AppState;
use crate::config_authority::events_error;

/// Audit identity of approvals made through the API.
const API_APPROVER: &str = "operator";

/// JSON for one consent row ([`remote_consent_view`], the view the CLI
/// prints too).
pub(crate) fn consent_json(row: &RemotePluginConsent) -> serde_json::Value {
    remote_consent_view(row)
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
    /// The `request_digest` of the request the operator reviewed, as the
    /// listing shows it (refused when the recorded request is another one).
    request_digest: String,
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
        &body.request_digest,
        &RemoteNarrowing {
            deny_domains: body.deny_domains,
            deny_bindings: body.deny_bindings,
            deny_network: body.deny_network,
        },
        RemoteApprover::Operator(API_APPROVER),
    )
    .await
    .map_err(|err| approval_error_response(&err))?;
    tracing::info!(
        cluster_id = %row.owner_cluster_id,
        plugin_key = %row.plugin_key,
        "owner plugin approved by this host's operator"
    );
    notify_consent_changed();
    Ok(Json(json!({ "consent": consent_json(&row) })))
}

/// The status and error code an approval refusal answers with: `404`
/// when nothing is recorded, `400` for a narrowing or stored request that
/// is not valid, `409` only when the request is not the reviewed one (or
/// changed under the write), and `503` for a store fault that may pass.
fn approval_status(err: &RemoteApprovalError) -> (StatusCode, &'static str) {
    match err {
        RemoteApprovalError::NotRequested(_) => (StatusCode::NOT_FOUND, "not_requested"),
        RemoteApprovalError::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid_approval"),
        RemoteApprovalError::Conflict(_) => (StatusCode::CONFLICT, "approval_refused"),
        RemoteApprovalError::Store(_) => (StatusCode::SERVICE_UNAVAILABLE, "store_unavailable"),
    }
}

/// The response for an approval refusal ([`approval_status`]). A store
/// fault's detail is logged, not returned.
fn approval_error_response(err: &RemoteApprovalError) -> Response {
    let (status, code) = approval_status(err);
    let detail = if let RemoteApprovalError::Store(detail) = err {
        tracing::warn!(error = %detail, "approval failed in the store");
        "the approval could not be stored; try again".to_string()
    } else {
        err.to_string()
    };
    (status, Json(json!({ "error": code, "detail": detail }))).into_response()
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

/// The live client of `cluster_id`'s link, running as the node the link
/// names now, or 503 `not_connected` (also while a re-join's new node has no
/// client yet).
async fn connected_link(
    state: &Arc<AppState>,
    cluster_id: &str,
) -> Result<bookclerk_node_client::HostClientHandle, Response> {
    let cluster_id = cluster_id.trim();
    let library = state.library_snapshot().await;
    let node_id = match load_cluster_link(&library, cluster_id).await {
        Ok(Some(link)) if link.is_active() => Some(link.remote_node_id),
        Ok(_) => None,
        Err(err) => return Err(events_error(err)),
    };
    node_id
        .and_then(|node_id| super::link_handle(cluster_id, &node_id))
        .ok_or_else(|| {
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
    State(state): State<Arc<AppState>>,
    axum::extract::Path(cluster_id): axum::extract::Path<String>,
    body: Option<Json<ClusterScanBody>>,
) -> Result<Json<serde_json::Value>, Response> {
    let body = body.map(|Json(body)| body).unwrap_or_default();
    // A blank account names nothing; dropping it would widen the scan to
    // every account of this node's owner.
    if body.account.as_deref().is_some_and(|a| a.trim().is_empty()) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid", "detail": "the account named is blank" })),
        )
            .into_response());
    }
    let handle = connected_link(&state, &cluster_id).await?;
    let answer = handle
        .request(bookclerk_node_proto::Body::ScanRequest {
            account: body.account,
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
    State(state): State<Arc<AppState>>,
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
    let handle = connected_link(&state, &cluster_id).await?;
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
