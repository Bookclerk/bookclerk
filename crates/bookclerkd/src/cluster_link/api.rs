//! Operator API for host consent (H-4 design, step 8):
//!
//! - `GET /api/config/cluster-consents`: what each owner cluster asks to
//!   run here and how this host stands on it;
//! - `POST /api/config/cluster-consents/approve`: approve one request by
//!   the `request_digest` the listing shows (refused with `409` when the
//!   recorded request is no longer that one), optionally narrowed (denied
//!   domains, bindings, or all network);
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
        "request_digest": row.request_digest(),
        "approved_version": row.version,
        "grant": parse(row.grant_json.as_deref()),
        "approved_at": row.approved_at,
        "approved_by": row.approved_by,
        "revoked_at": row.revoked_at,
        "updated_at": row.updated_at,
        "state": consent_state(row).as_str(),
    })
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
