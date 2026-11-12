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
    approve_remote_request, remote_row_changes, remote_row_consent_state, RemoteApprovalError,
    RemoteNarrowing,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::consent::notify_consent_changed;
use crate::api::AppState;
use crate::config_authority::events_error;

/// Audit identity of approvals made through the API.
const API_APPROVER: &str = "operator";

/// The view of one consent row: the same fields, in the same shapes, as
/// [`bookclerk_plugin_host::remote_consent_view`], which the CLI prints
/// (a test keeps the two equal). A stored grant that does not parse shows
/// as `null`.
pub(crate) fn consent_json(row: &RemotePluginConsent) -> RemoteConsentView {
    let parse = |raw: Option<&str>| {
        raw.and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .unwrap_or(serde_json::Value::Null)
    };
    RemoteConsentView {
        cluster_id: row.owner_cluster_id.clone(),
        plugin_key: row.plugin_key.clone(),
        requested_version: row.requested_version.clone(),
        requested_grant: parse(Some(&row.requested_grant_json)),
        requested_artifact: RequestedArtifact {
            url: row.requested_artifact_url.clone(),
            archive_sha256: row.requested_artifact_sha256.clone(),
            payload_root_sha256: row.requested_artifact_payload_sha256.clone(),
        },
        request_digest: row.request_digest(),
        approved_version: row.version.clone(),
        grant: parse(row.grant_json.as_deref()),
        approved_at: row.approved_at.clone(),
        approved_by: row.approved_by.clone(),
        revoked_at: row.revoked_at.clone(),
        updated_at: row.updated_at.clone(),
        state: remote_row_consent_state(row).as_str(),
        pending: remote_row_changes(row),
    }
}

/// JSON for one cluster link: ids, origin and state, never key material.
pub(crate) fn link_json(link: &ClusterLink) -> ClusterLinkView {
    ClusterLinkView {
        cluster_id: link.owner_cluster_id.clone(),
        owner_origin: link.owner_origin.clone(),
        node_id: link.remote_node_id.clone(),
        pinned: link.spki_pin.is_some(),
        state: link.state.clone(),
        enrolled_at: link.enrolled_at.clone(),
        share_books: link.share_books,
        include_branches: link.include_branches,
        work_for_members: link.work_for_members,
        keep_member_copies: link.keep_member_copies,
        max_jobs: link.max_jobs.map(|cap| cap.jobs()),
        job_share: None,
        list_members_books: link.list_members_books,
        running: None,
    }
}

/// [`link_json`] with the link's current share of this machine.
pub(crate) fn shared_link_json(state: &AppState, link: &ClusterLink) -> ClusterLinkView {
    let mut view = link_json(link);
    view.job_share = state.capacity.share_of(&link.owner_cluster_id);
    view
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// A library this install is a branch of: ids, origin and state, never key material.
pub struct ClusterLinkView {
    /// The library's cluster id.
    #[schema(example = "c1f2e3d4-5a6b-4c7d-8e9f-0a1b2c3d4e5f")]
    pub cluster_id: String,
    /// The library's canonical origin.
    #[schema(example = "https://books.example.com")]
    pub owner_origin: String,
    /// The node id the library minted for this install.
    #[schema(example = "7d2b9e4c-1f3a-4b8d-9c6e-2a5f8b1d4e7c")]
    pub node_id: String,
    /// Whether the link pins the library's self-signed certificate.
    #[schema(example = false)]
    pub pinned: bool,
    /// `active`, `revoked`, or `left`.
    #[schema(example = "active")]
    pub state: String,
    /// RFC 3339 enrollment time.
    #[schema(example = "2026-10-29T09:02:41Z")]
    pub enrolled_at: String,
    /// "Share this branch's books with the library".
    #[schema(example = true)]
    pub share_books: bool,
    /// "Include books from my own branches".
    #[schema(example = true)]
    pub include_branches: bool,
    /// "Download for other members of the library": this install takes the library's work for its other members too, not only its owner's (off by default).
    #[schema(example = false)]
    pub work_for_members: bool,
    /// "Keep copies of other members' books": a book acquired here for another member is also written to this install's own destinations (off by default).
    #[schema(example = false)]
    pub keep_member_copies: bool,
    /// The most jobs this install runs for the library at once, as its operator set it; `null` gives the library an equal share of the machine.
    #[schema(example = 2, minimum = 0, maximum = 16)]
    pub max_jobs: Option<usize>,
    /// How many jobs the library may have running here now: its share of the machine after this install's own work, within `max_jobs`. Only the listing and sharing routes report it, and only while the link's client runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 2)]
    pub job_share: Option<usize>,
    /// "List my members' books to the library": the library's catalog
    /// lookups are answered with members' books (except those who opted
    /// out) as well as the owner's.
    #[schema(example = true)]
    pub list_members_books: bool,
    /// Whether this install's client for the link is running now; only the join routes report it.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = true)]
    pub running: Option<bool>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// An owner library's request to run a plugin on this install, and what the operator answered.
pub struct RemoteConsentView {
    /// The owner library's cluster id.
    #[schema(example = "c1f2e3d4-5a6b-4c7d-8e9f-0a1b2c3d4e5f")]
    pub cluster_id: String,
    /// Canonical plugin key.
    #[schema(example = "bookclerk:source-libro")]
    pub plugin_key: String,
    /// Version the owner deploys here now.
    #[schema(example = "1.4.2")]
    pub requested_version: String,
    /// The owner's approved grant for that version.
    #[schema(schema_with = grant_schema)]
    pub requested_grant: serde_json::Value,
    /// The archive the owner deploys to this install.
    pub requested_artifact: RequestedArtifact,
    /// Digest of the request as a whole; an approval names it so a changed request is not approved by mistake.
    #[schema(example = "9c1e4f7a2b3d5e6f8a9b0c1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6a7b8c9d0e1f")]
    pub request_digest: String,
    /// Version this install's operator approved, when any.
    #[schema(example = "1.4.2")]
    pub approved_version: Option<String>,
    /// The grant the operator consented to, when any.
    #[schema(schema_with = grant_schema)]
    pub grant: serde_json::Value,
    /// RFC 3339 approval time.
    #[schema(example = "2026-10-29T10:12:00Z")]
    pub approved_at: Option<String>,
    /// Who approved (`operator`, or `platform` for an automatic approval).
    #[schema(example = "operator")]
    pub approved_by: Option<String>,
    /// RFC 3339 time the operator revoked the approval.
    #[schema(example = "2026-11-01T08:00:00Z")]
    pub revoked_at: Option<String>,
    /// RFC 3339 time of the last write.
    #[schema(example = "2026-10-29T10:12:00Z")]
    pub updated_at: String,
    /// How this host stands on the request: `consented`, `awaiting_host_consent` (no approval, or a revoked one), `awaiting_host_reapproval` (the owner changed the request or the archive since the approval), or `invalid_grant` (a grant that does not parse).
    #[schema(example = "consented")]
    pub state: &'static str,
    /// What the owner's current request asks for beyond the request the operator approved, as the operator-facing lines the CLI prints; empty when nothing was approved yet or the request stays within the approval.
    #[schema(example = json!(["Awaiting consent (not automatically approved):", "  fetch hosts: any public website (*)"]))]
    pub pending: Vec<String>,
}

/// Schema for the grant members of [`RemoteConsentView`].
fn grant_schema() -> utoipa::openapi::schema::Schema {
    utoipa::openapi::ObjectBuilder::new()
        .schema_type(utoipa::openapi::schema::SchemaType::AnyValue)
        .description(Some(
            "A plugin grant document (the plugin host's `PluginGrant`): the plugin key, the entrypoints, network mode and domains, and the bindings it may use; `null` when nothing was approved.",
        ))
        .examples([json!({
            "schemaVersion": 2,
            "pluginKey": "bookclerk:source-libro",
            "pluginId": "libro",
            "entrypoints": ["storefront"],
            "producers": [],
            "consumers": [],
            "jobs": [],
            "networkMode": "outbound",
            "domains": ["api.libro.fm", "cdn.libro.fm"],
            "manifestDomains": ["api.libro.fm", "cdn.libro.fm"],
            "operatorAddedDomains": [],
            "operatorDeniedDomains": [],
            "tcp": [],
            "manifestTcp": [],
            "operatorAddedTcp": [],
            "operatorDeniedTcp": [],
            "addressCidrs": [],
            "manifestCidrs": [],
            "operatorAddedCidrs": [],
            "operatorDeniedCidrs": [],
            "allowUndeclaredPublicRedirects": false,
            "bindings": ["config", "secrets"],
            "compatibilityFlags": [],
            "approvedAt": "2026-10-29T10:12:00Z"
        })])
        .build()
        .into()
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// The archive an owner library deploys to this install.
pub struct RequestedArtifact {
    /// Where the owner fetches it from.
    #[schema(example = "https://books.example.com/api/plugins/artifacts/9c1e4f7a")]
    pub url: String,
    /// SHA-256 of the archive.
    #[schema(example = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")]
    pub archive_sha256: String,
    /// Payload-root digest of the tree it unpacks to.
    #[schema(example = "a7ffc6f8bf1ed76651c14756a061d662f580ff4de43b49fa82d80a4b80f8434a")]
    pub payload_root_sha256: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// JSON body for `GET /api/config/cluster-consents`.
pub struct ClusterConsentsResponse {
    /// Every library this install is a branch of.
    pub clusters: Vec<ClusterLinkView>,
    /// Every plugin request from those libraries, pending or answered.
    pub consents: Vec<RemoteConsentView>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// JSON body after a consent was approved or revoked.
pub struct ConsentChanged {
    /// The consent as stored now.
    pub consent: RemoteConsentView,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// JSON body after the owner library queued a scan for this install.
pub struct ClusterScanPlanned {
    /// The owner library's cluster id.
    #[schema(example = "c1f2e3d4-5a6b-4c7d-8e9f-0a1b2c3d4e5f")]
    pub cluster_id: String,
    /// Ids of the jobs the owner created.
    #[schema(example = json!(["scan-7f3a9c1e"]))]
    pub created: Vec<String>,
    /// Ids of jobs that were already pending or running.
    #[schema(example = json!([]))]
    pub duplicates: Vec<String>,
    /// Planned jobs the owner's queue had no room for.
    #[schema(example = 0)]
    pub queue_full: u64,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
/// JSON body for `POST /api/config/clusters/{cluster_id}/jobs`.
pub struct ClusterJobsResponse {
    /// The owner's state of each job this install may see, in request order.
    pub jobs: Vec<bookclerk_node_proto::JobStatusEntry>,
}

/// `GET /api/config/cluster-consents`.
#[utoipa::path(
    get,
    path = "/api/config/cluster-consents",
    responses(
        (status = 200, description = "Pending and granted cluster consents.", body = ClusterConsentsResponse),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The session is impersonating a user."),
        (status = 500, description = "The library database could not be read.", body = crate::http_error::ErrorMessage, example = json!({"error": "ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection"})),
    ),
)]
pub async fn get_cluster_consents(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ClusterConsentsResponse>, Response> {
    let library = state.library_snapshot().await;
    let links = list_cluster_links(&library).await.map_err(events_error)?;
    let rows = list_remote_plugin_consents(&library, None)
        .await
        .map_err(events_error)?;
    Ok(Json(ClusterConsentsResponse {
        clusters: links
            .iter()
            .map(|link| shared_link_json(&state, link))
            .collect(),
        consents: rows.iter().map(consent_json).collect(),
    }))
}

/// `POST /api/config/cluster-consents/approve` body.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApproveBody {
    /// Owner cluster id.
    #[schema(example = "6d1f2e3a-4b5c-4d6e-8f7a-9b0c1d2e3f4a")]
    cluster_id: String,
    /// Canonical plugin key.
    #[schema(example = "platform:bookclerk/bookclerk-plugin-source-libro#libro")]
    plugin_key: String,
    /// The `request_digest` of the request the operator reviewed, as the
    /// listing shows it (refused when the recorded request is another one).
    #[schema(example = "9b74c9897bac770ffc029102a200c5de3a9c4a1d6c2f1e0b5a7d8c9e0f1a2b3c")]
    request_digest: String,
    /// Domains to take out of the request.
    #[serde(default)]
    #[schema(example = json!(["telemetry.example.com"]))]
    deny_domains: BTreeSet<String>,
    /// Bindings to take out of the request.
    #[serde(default)]
    #[schema(example = json!(["oauth"]))]
    deny_bindings: BTreeSet<String>,
    /// Requested hosts to refuse the plugin's own User-Agent on; requests to
    /// them carry Bookclerk's agent instead.
    #[serde(default)]
    #[schema(example = json!(["*.libro.fm"]))]
    deny_own_user_agent_hosts: BTreeSet<String>,
    /// Deny all network access.
    #[serde(default)]
    #[schema(example = false)]
    deny_network: bool,
}

/// `POST /api/config/cluster-consents/approve`.
#[utoipa::path(
    post,
    path = "/api/config/cluster-consents/approve",
    request_body = ApproveBody,
    responses(
        (status = 200, description = "The consent was granted.", body = ConsentChanged),
        (status = 400, description = "The narrowing or the recorded request is not valid (`invalid_approval`).", body = crate::http_error::ErrorDetail, example = json!({"error": "invalid_approval", "detail": "the recorded request does not parse"})),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The session is impersonating a user."),
        (status = 404, description = "That cluster has not asked to run that plugin here (`not_requested`).", body = crate::http_error::ErrorDetail, example = json!({"error": "not_requested", "detail": "cluster c1f2e3d4-5a6b-4c7d-8e9f-0a1b2c3d4e5f has not asked to run bookclerk:source-libro here"})),
        (status = 409, description = "The request changed since it was reviewed; review it again (`approval_refused`).", body = crate::http_error::ErrorDetail, example = json!({"error": "approval_refused", "detail": "the request for bookclerk:source-libro changed since it was reviewed (now version 1.4.3, request 9c1e4f7a2b3d5e6f8a9b0c1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6a7b8c9d0e1f); review it again"})),
        (status = 503, description = "The approval could not be stored; try again (`store_unavailable`).", body = crate::http_error::ErrorDetail, example = json!({"error": "store_unavailable", "detail": "the approval could not be stored; try again"})),
    ),
)]
pub async fn post_cluster_consent_approve(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ApproveBody>,
) -> Result<Json<ConsentChanged>, Response> {
    let library = state.library_snapshot().await;
    let row = approve_remote_request(
        &library,
        body.cluster_id.trim(),
        body.plugin_key.trim(),
        &body.request_digest,
        &RemoteNarrowing {
            deny_domains: body.deny_domains,
            deny_bindings: body.deny_bindings,
            deny_own_user_agent_hosts: body.deny_own_user_agent_hosts,
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
    Ok(Json(ConsentChanged {
        consent: consent_json(&row),
    }))
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
    (
        status,
        Json(crate::http_error::ErrorDetail::new(code, detail)),
    )
        .into_response()
}

/// `POST /api/config/cluster-consents/revoke` body.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RevokeBody {
    /// Owner cluster id.
    #[schema(example = "6d1f2e3a-4b5c-4d6e-8f7a-9b0c1d2e3f4a")]
    cluster_id: String,
    /// Canonical plugin key.
    #[schema(example = "platform:bookclerk/bookclerk-plugin-source-libro#libro")]
    plugin_key: String,
}

/// `POST /api/config/cluster-consents/revoke`.
#[utoipa::path(
    post,
    path = "/api/config/cluster-consents/revoke",
    request_body = RevokeBody,
    responses(
        (status = 200, description = "The consent was revoked.", body = ConsentChanged),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The session is impersonating a user."),
        (status = 404, description = "No such consent (`not_found`).", body = crate::http_error::ErrorMessage, example = json!({"error": "not_found"})),
        (status = 500, description = "The library database could not be read or written.", body = crate::http_error::ErrorMessage, example = json!({"error": "ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection"})),
    ),
)]
pub async fn post_cluster_consent_revoke(
    State(state): State<Arc<AppState>>,
    Json(body): Json<RevokeBody>,
) -> Result<Json<ConsentChanged>, Response> {
    let library = state.library_snapshot().await;
    let row = revoke_remote_plugin(&library, body.cluster_id.trim(), body.plugin_key.trim())
        .await
        .map_err(events_error)?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(crate::http_error::ErrorMessage::new("not_found")),
            )
                .into_response()
        })?;
    tracing::info!(
        cluster_id = %row.owner_cluster_id,
        plugin_key = %row.plugin_key,
        "owner plugin consent revoked by this host's operator"
    );
    notify_consent_changed();
    Ok(Json(ConsentChanged {
        consent: consent_json(&row),
    }))
}

/// `POST /api/config/clusters/{cluster_id}/scan` body.
#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ClusterScanBody {
    /// Scan only this store account of the owner cluster.
    #[serde(default)]
    #[schema(example = "reader@example.com")]
    account: Option<String>,
    /// Pin every planned job to this owner plugin instance.
    #[serde(default)]
    #[schema(example = "3f9c2b7e-1d4a-4c8e-9b2f-6a7d8e9f0a1b")]
    plugin_instance_id: Option<String>,
}

/// `POST /api/config/clusters/{cluster_id}/jobs` body.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ClusterJobsBody {
    /// Job ids on the owner cluster.
    #[schema(example = json!(["acquire-3f9c2b7e-1d4a-4c8e-9b2f-6a7d8e9f0a1b"]))]
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
                Json(crate::http_error::ErrorDetail::new(
                    "not_connected",
                    "this daemon has no running link to that owner cluster",
                )),
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
            (
                status,
                Json(crate::http_error::ErrorDetail::new(code, message)),
            )
                .into_response()
        }
        Some(_) => (
            StatusCode::BAD_GATEWAY,
            Json(crate::http_error::ErrorMessage::new("unexpected_answer")),
        )
            .into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(crate::http_error::ErrorMessage::new("owner_unavailable")),
        )
            .into_response(),
    }
}

/// `POST /api/config/clusters/{cluster_id}/scan`: asks the owner cluster
/// to queue a scan for this node's owner (H-5). The owner's planner decides
/// what that user may scan; the answer lists the queued job ids.
#[utoipa::path(
    post,
    path = "/api/config/clusters/{cluster_id}/scan",
    params(("cluster_id" = String, Path, description = "Owner cluster id.")),
    request_body = Option<ClusterScanBody>,
    responses(
        (status = 200, description = "The owner cluster accepted the scan.", body = ClusterScanPlanned),
        (status = 400, description = "The account named is blank, or the owner refused the request as invalid (`invalid`).", body = crate::http_error::ErrorDetail, examples(
            ("blank_account" = (summary = "The body names a blank account.", value = json!({"error": "invalid", "detail": "the account named is blank"}))),
            ("owner_refused" = (summary = "The owner cluster refused the request as invalid.", value = json!({"error": "invalid", "detail": "account libro:reader@example.com is not on this node's owner"}))),
        )),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The owner refused the scan (`forbidden`), or the session is impersonating a user (the standard error body).", body = crate::http_error::ErrorAnswer, examples(
            ("owner_refused" = (summary = "The owner cluster refused the scan.", value = json!({"error": "forbidden", "detail": "this node may not queue library work"}))),
            ("impersonating" = (summary = "The session is impersonating a user.", value = json!({"error": "forbidden", "message": "You do not have access to this resource.", "status": 403}))),
        )),
        (status = 502, description = "The owner cluster answered with an unexpected frame (`unexpected_answer`), or with an error code other than `forbidden` and `invalid`.", body = crate::http_error::ErrorAnswer, examples(
            ("unexpected_answer" = (summary = "The owner answered with a frame that is not a scan plan.", value = json!({"error": "unexpected_answer"}))),
            ("owner_error" = (summary = "The owner answered with another error code.", value = json!({"error": "unavailable", "detail": "the scan queue is full"}))),
        )),
        (status = 503, description = "This daemon has no running link to that cluster (`not_connected`), or the owner did not answer (`owner_unavailable`).", body = crate::http_error::ErrorAnswer, examples(
            ("not_connected" = (summary = "No link to that cluster is running (or no such cluster).", value = json!({"error": "not_connected", "detail": "this daemon has no running link to that owner cluster"}))),
            ("owner_unavailable" = (summary = "The link is up but the owner did not answer in time.", value = json!({"error": "owner_unavailable"}))),
        )),
        (status = 500, description = "The library database could not be read.", body = crate::http_error::ErrorMessage, example = json!({"error": "ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection"})),
    ),
)]
pub async fn post_cluster_scan(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(cluster_id): axum::extract::Path<String>,
    body: Option<Json<ClusterScanBody>>,
) -> Result<Json<ClusterScanPlanned>, Response> {
    let body = body.map(|Json(body)| body).unwrap_or_default();
    // A blank account names nothing; dropping it would widen the scan to
    // every account of this node's owner.
    if body.account.as_deref().is_some_and(|a| a.trim().is_empty()) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(crate::http_error::ErrorDetail::new(
                "invalid",
                "the account named is blank",
            )),
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
            Ok(Json(ClusterScanPlanned {
                cluster_id: handle.cluster_id().to_owned(),
                created,
                duplicates,
                queue_full,
            }))
        }
        other => Err(owner_refusal(other)),
    }
}

/// `POST /api/config/clusters/{cluster_id}/jobs`: the owner's view of jobs
/// this node may see (those it would be trusted to claim).
#[utoipa::path(
    post,
    path = "/api/config/clusters/{cluster_id}/jobs",
    params(("cluster_id" = String, Path, description = "Owner cluster id.")),
    request_body = ClusterJobsBody,
    responses(
        (status = 200, description = "The owner's jobs for these accounts.", body = ClusterJobsResponse),
        (status = 400, description = "Too many job ids (`too_many_jobs`), or the owner refused the request as invalid (`invalid`).", body = crate::http_error::ErrorAnswer, examples(
            ("too_many_jobs" = (summary = "The body lists more job ids than one request may carry.", value = json!({"error": "too_many_jobs"}))),
            ("owner_refused" = (summary = "The owner cluster refused the request as invalid.", value = json!({"error": "invalid", "detail": "too many job ids"}))),
        )),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The owner refused the request (`forbidden`), or the session is impersonating a user (the standard error body).", body = crate::http_error::ErrorAnswer, examples(
            ("owner_refused" = (summary = "The owner cluster refused the request.", value = json!({"error": "forbidden", "detail": "this node may not read library work"}))),
            ("impersonating" = (summary = "The session is impersonating a user.", value = json!({"error": "forbidden", "message": "You do not have access to this resource.", "status": 403}))),
        )),
        (status = 502, description = "The owner cluster answered with an unexpected frame (`unexpected_answer`), or with an error code other than `forbidden` and `invalid`.", body = crate::http_error::ErrorAnswer, examples(
            ("unexpected_answer" = (summary = "The owner answered with a frame that is not a job status.", value = json!({"error": "unexpected_answer"}))),
            ("owner_error" = (summary = "The owner answered with another error code.", value = json!({"error": "unavailable", "detail": "the job store is busy"}))),
        )),
        (status = 503, description = "This daemon has no running link to that cluster (`not_connected`), or the owner did not answer (`owner_unavailable`).", body = crate::http_error::ErrorAnswer, examples(
            ("not_connected" = (summary = "No link to that cluster is running (or no such cluster).", value = json!({"error": "not_connected", "detail": "this daemon has no running link to that owner cluster"}))),
            ("owner_unavailable" = (summary = "The link is up but the owner did not answer in time.", value = json!({"error": "owner_unavailable"}))),
        )),
        (status = 500, description = "The library database could not be read.", body = crate::http_error::ErrorMessage, example = json!({"error": "ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection"})),
    ),
)]
pub async fn post_cluster_jobs(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(cluster_id): axum::extract::Path<String>,
    Json(body): Json<ClusterJobsBody>,
) -> Result<Json<ClusterJobsResponse>, Response> {
    if body.job_ids.len() > bookclerk_node_proto::MAX_STATUS_JOBS {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(crate::http_error::ErrorMessage::new("too_many_jobs")),
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
        }) => Ok(Json(ClusterJobsResponse { jobs })),
        other => Err(owner_refusal(other)),
    }
}

#[cfg(test)]
#[path = "api_tests.rs"]
mod tests;
