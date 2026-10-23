//! `GET /api/node/v1/artifacts/{sha256}`: owner plugin archives for host
//! nodes (H-4 design, `artifact.get`).
//!
//! Like the node socket, the route sits outside the cookie and operator
//! auth layers: the node's enrolled Ed25519 key is the credential, carried
//! as `Authorization: Bookclerk-Node <node_id> <issued_at> <sig>` over
//! [`bookclerk_node_proto::artifact_transcript`] (cluster, node, owner
//! origin, archive digest, issue time; [`ARTIFACT_REQUEST_MAX_SKEW_SECS`]
//! either way). The transport policy runs first. A node gets only `blob:`
//! archives of versions installed for deployments it is trusted with
//! ([`artifact_served_to_node`]); every refusal answers the same 404 so the
//! route never says which archives exist.
//!
//! [`ARTIFACT_REQUEST_MAX_SKEW_SECS`]: bookclerk_node_proto::ARTIFACT_REQUEST_MAX_SKEW_SECS

use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bookclerk_library::control_plane::{
    artifact_served_to_node, load_artifact_blob, load_cluster_row, load_node_credential,
};
use bookclerk_node_proto::{
    canonical_origin, parse_artifact_authorization, verify_artifact_request, ArtifactRequest,
};
use serde_json::json;

use super::check_transport;
use crate::api::AppState;

/// The one refusal every failed artifact request gets.
fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "artifact_unavailable" })),
    )
        .into_response()
}

/// True for a lowercase hex SHA-256.
fn is_sha256_hex(raw: &str) -> bool {
    raw.len() == 64 && raw.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `GET /api/node/v1/artifacts/{sha256}`.
#[utoipa::path(
    get,
    path = "/api/node/v1/artifacts/{sha256}",
    params(("sha256" = String, Path, description = "Archive digest, lowercase hex.")),
    responses(
        (status = 200, description = "The plugin archive.", content_type = "application/octet-stream"),
        (status = 401, description = "The node signature is missing or wrong."),
        (status = 404, description = "No such archive, or the node API is not mounted."),
    ),
)]
pub async fn get_artifact(
    State(state): State<Arc<AppState>>,
    Path(sha256): Path<String>,
    request: Request,
) -> Response {
    let (mut parts, _body) = request.into_parts();
    if let Err(refused) = check_transport(&state, &mut parts, "artifact").await {
        return refused;
    }
    if !is_sha256_hex(&sha256) {
        return not_found();
    }
    let Some((node_id, issued_at, sig)) = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_artifact_authorization)
    else {
        return not_found();
    };
    let library = state.library_snapshot().await;
    let origin = {
        let cfg = state.config.read().await;
        canonical_origin(&crate::origin::effective_origin_from_config(&cfg, None))
    };
    let Some(origin) = origin else {
        return not_found();
    };
    let Ok(Some(cluster)) = load_cluster_row(&library).await else {
        return not_found();
    };
    let credential = match load_node_credential(&library, node_id).await {
        Ok(Some(credential)) if credential.revoked_at.is_none() => credential,
        _ => return not_found(),
    };
    let artifact_request = ArtifactRequest {
        cluster_id: &cluster.cluster_id,
        node_id,
        owner_origin: &origin,
        archive_sha256: &sha256,
        issued_at,
    };
    let now = chrono::Utc::now().timestamp();
    if verify_artifact_request(&credential.sign_public_key, &artifact_request, sig, now).is_err() {
        tracing::warn!(node_id, "artifact request refused: bad signature");
        return not_found();
    }
    match artifact_served_to_node(&library, node_id, &sha256).await {
        Ok(true) => {}
        Ok(false) => return not_found(),
        Err(err) => {
            tracing::warn!(node_id, error = %err, "artifact scope check failed");
            return not_found();
        }
    }
    match load_artifact_blob(&library, &sha256).await {
        Ok(Some(bytes)) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Ok(None) => not_found(),
        Err(err) => {
            tracing::warn!(node_id, error = %err, "artifact read failed");
            not_found()
        }
    }
}
