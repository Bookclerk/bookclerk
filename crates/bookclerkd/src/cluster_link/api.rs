//! Operator API for host consent (H-4 design, step 8):
//!
//! - `GET /api/config/cluster-consents`: what each owner cluster asks to
//!   run here (version, grant, and the archive URL and digest this host
//!   would fetch) and how this host stands on it;
//! - `POST /api/config/cluster-consents/approve`: approve one request by
//!   the `request_digest` the listing shows (refused with `409` when the
//!   recorded request is no longer that one, `404` when none is recorded,
//!   `400` for a narrowing it does not ask for, `503` on a store fault),
//!   optionally narrowed (denied domains, bindings, or all network);
//! - `POST /api/config/cluster-consents/revoke`: withdraw an approval (a
//!   revoke holds: a platform plugin is not approved automatically again).
//!
//! All three are operator-only: owner plugins run under this install's
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
    list_remote_plugin_consents, revoke_remote_plugin, RemoteApprover, RemotePluginConsent,
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

/// `GET /api/config/cluster-consents`.
pub async fn get_cluster_consents(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, Response> {
    let library = state.library_snapshot().await;
    let rows = list_remote_plugin_consents(&library, None)
        .await
        .map_err(events_error)?;
    Ok(Json(json!({
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a changed request is a conflict; a missing request, a bad
    /// narrowing and a store fault each answer their own status.
    #[test]
    fn approval_refusals_answer_by_cause() {
        let cases = [
            (
                RemoteApprovalError::NotRequested("x".into()),
                StatusCode::NOT_FOUND,
            ),
            (
                RemoteApprovalError::Invalid("x".into()),
                StatusCode::BAD_REQUEST,
            ),
            (
                RemoteApprovalError::Conflict("x".into()),
                StatusCode::CONFLICT,
            ),
            (
                RemoteApprovalError::Store("x".into()),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        ];
        for (err, status) in cases {
            assert_eq!(approval_status(&err).0, status, "{err:?}");
            assert_eq!(approval_error_response(&err).status(), status);
        }
    }
}
