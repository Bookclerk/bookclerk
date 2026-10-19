//! Host-node enrollment over HTTP.
//!
//! `/api/config/node-invites` lets a signed-in user mint, list, and revoke
//! enrollment links (the library decides who may act for whom). A minted
//! link's secret appears once, in the `url` fragment of the mint response;
//! it is never logged.
//!
//! `DELETE /api/config/node-credentials/{node_id}` revokes an enrolled
//! host's credential: the operator or an Owner for any host, anyone else
//! for the hosts they own.
//!
//! `/api/config/host-enrollment` reads and toggles the cluster's
//! host-enrollment switch (`core.host_enrollment`). Any signed-in user may
//! read it; only the operator or an active Owner may change it.
//!
//! `POST /api/node/v1/enroll` redeems a link. It sits outside the cookie and
//! operator auth layers (the link secret is the credential) and is never
//! disabled by `auth.enabled = false`; [`transport::classify_transport`]
//! runs before the body is read.

pub mod transport;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::connect_info::ConnectInfo;
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use bookclerk_library::control_plane::{
    list_enrollment_links, load_host_enrollment, mint_enrollment_link, redeem_enrollment_link,
    revoke_enrollment_link, revoke_node_credential, set_host_enrollment_enabled,
    verify_cluster_owner, ConfigActor, EnrollmentActor, EnrollmentLink, EnrollmentSecret,
    NodeCredential, RedeemRequest,
};
use bookclerk_library::LibraryError;
use serde::Deserialize;
use serde_json::json;

use self::transport::{
    classify_transport, refused_response, ConnInfo, ListenerKind, LocalAddr, TransportConfig,
    TransportDecision,
};

use crate::api::AppState;
use crate::auth::UserCaller;
use crate::config_authority::events_error;

/// Library actor for an HTTP caller.
fn actor(caller: UserCaller) -> EnrollmentActor {
    match caller {
        UserCaller::Operator => EnrollmentActor::Operator {
            id: "operator".into(),
        },
        UserCaller::User { user_id } => EnrollmentActor::User { user_id },
    }
}

/// JSON for one link; never the secret or its hash.
fn link_json(link: &EnrollmentLink) -> serde_json::Value {
    json!({
        "link_id": link.link_id,
        "for_user_id": link.for_user_id,
        "label": link.label,
        "created_by": link.created_by,
        "created_at": link.created_at,
        "expires_at": link.expires_at,
        "redeemed_at": link.redeemed_at,
        "redeemed_node_id": link.redeemed_node_id,
        "revoked_at": link.revoked_at,
        "state": link.state(chrono::Utc::now()),
    })
}

/// `POST /api/config/node-invites` body.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PostNodeInvite {
    /// User who will own the node. Defaults to the caller; the operator
    /// must name one.
    #[serde(default)]
    user_id: Option<i64>,
    /// Link lifetime in seconds (default 24 h, 5 min to 7 days).
    #[serde(default)]
    ttl_secs: Option<u64>,
    /// Label shown in listings.
    #[serde(default)]
    label: String,
}

/// `POST /api/config/node-invites`: mint an enrollment link. Any user may
/// mint one for themselves while host enrollment is on; the operator or an
/// Owner may mint for any active user.
pub async fn post_node_invite(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    headers: HeaderMap,
    Json(body): Json<PostNodeInvite>,
) -> Result<Json<serde_json::Value>, Response> {
    let for_user_id = match (body.user_id, caller) {
        (Some(user_id), _) | (None, UserCaller::User { user_id }) => user_id,
        (None, UserCaller::Operator) => {
            return Err(events_error(bookclerk_library::LibraryError::Invalid(
                "user_id is required: the operator has no user account".into(),
            )))
        }
    };
    let library = state.library_snapshot().await;
    let minted = mint_enrollment_link(
        &library,
        &actor(caller),
        for_user_id,
        &body.label,
        body.ttl_secs.map(std::time::Duration::from_secs),
    )
    .await
    .map_err(events_error)?;
    let origin = {
        let cfg = state.config.read().await;
        crate::origin::effective_origin_from_config(&cfg, Some(&headers))
    };
    let url = minted.url(&origin, None);
    Ok(Json(json!({
        "link": link_json(&minted.link),
        "url": url.as_str(),
    })))
}

/// `GET /api/config/node-invites`: every link for the operator or an
/// Owner; the caller's own links for anyone else.
pub async fn get_node_invites(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
) -> Result<Json<serde_json::Value>, Response> {
    let library = state.library_snapshot().await;
    let links = list_enrollment_links(&library, &actor(caller))
        .await
        .map_err(events_error)?;
    Ok(Json(json!({
        "links": links.iter().map(link_json).collect::<Vec<_>>(),
    })))
}

/// `DELETE /api/config/node-invites/{id}`: revoke an unredeemed link.
pub async fn delete_node_invite(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    Path(link_id): Path<String>,
) -> Result<Json<serde_json::Value>, Response> {
    let library = state.library_snapshot().await;
    let link = revoke_enrollment_link(&library, &actor(caller), link_id.trim())
        .await
        .map_err(events_error)?;
    Ok(Json(json!({ "link": link_json(&link) })))
}

/// JSON for one host credential; public keys only.
fn credential_json(credential: &NodeCredential) -> serde_json::Value {
    json!({
        "node_id": credential.node_id,
        "link_id": credential.link_id,
        "enrolled_at": credential.enrolled_at,
        "revoked_at": credential.revoked_at,
        "last_connected_at": credential.last_connected_at,
    })
}

/// `DELETE /api/config/node-credentials/{node_id}`: revoke an enrolled
/// host's credential so it can no longer connect.
///
/// # Arguments
///
/// * `caller` - Who is asking; the operator or an Owner may revoke any host,
///   any other active user only the hosts they own.
/// * `node_id` - The host node to revoke.
///
/// # Returns
///
/// `{credential}` with `revoked_at` set; revoking twice keeps the first
/// time.
///
/// # Errors
///
/// Returns 404 when the node is unknown or not the caller's, 400 when the
/// node has no host credential, 403 for a disabled user, and an error
/// response when the write fails.
pub async fn delete_node_credential(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    Path(node_id): Path<String>,
) -> Result<Json<serde_json::Value>, Response> {
    let library = state.library_snapshot().await;
    let credential = revoke_node_credential(&library, &actor(caller), node_id.trim())
        .await
        .map_err(events_error)?;
    Ok(Json(json!({ "credential": credential_json(&credential) })))
}

/// `PUT /api/config/host-enrollment` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PutHostEnrollment {
    /// Whether host nodes may enroll into this cluster.
    enabled: bool,
}

/// `GET /api/config/host-enrollment`: whether host nodes may enroll.
///
/// # Returns
///
/// `{enabled, revision}`; `revision` is `null` while the default (enabled)
/// applies.
///
/// # Errors
///
/// Returns an error response when the switch cannot be read.
pub async fn get_host_enrollment(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, Response> {
    let library = state.library_snapshot().await;
    let settings = load_host_enrollment(&library).await.map_err(events_error)?;
    Ok(Json(json!({
        "enabled": settings.body.enabled,
        "revision": settings.revision,
    })))
}

/// `PUT /api/config/host-enrollment`: turn host enrollment on or off. The
/// operator may always; a user only while the stored role is an active
/// Owner.
///
/// # Arguments
///
/// * `caller` - Who is asking; a user's role is read from the database.
/// * `body` - The new switch value.
///
/// # Returns
///
/// `{enabled, revision}` as stored.
///
/// # Errors
///
/// Returns 403 when the caller is neither the operator nor an active Owner,
/// and an error response when the switch cannot be written.
pub async fn put_host_enrollment(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<UserCaller>,
    Json(body): Json<PutHostEnrollment>,
) -> Result<Json<serde_json::Value>, Response> {
    let library = state.library_snapshot().await;
    let config_actor = match caller {
        UserCaller::Operator => ConfigActor::Operator {
            id: "operator".into(),
        },
        UserCaller::User { user_id } => verify_cluster_owner(&library, user_id)
            .await
            .map_err(events_error)?,
    };
    let doc = set_host_enrollment_enabled(&library, &config_actor, body.enabled)
        .await
        .map_err(events_error)?;
    Ok(Json(json!({
        "enabled": doc.body.enabled,
        "revision": doc.revision,
    })))
}

/// Largest enroll request body.
const MAX_ENROLL_BODY_BYTES: usize = 16 * 1024;

/// `POST /api/node/v1/enroll` body. Keys are base64url without padding.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnrollBody {
    /// Link id from the token.
    link_id: String,
    /// Link secret from the token.
    secret: String,
    /// Ed25519 public signing key.
    sign_pk: String,
    /// X25519 public box key.
    box_pk: String,
    /// Host's Bookclerk version.
    #[serde(default)]
    software_version: String,
}

/// JSON error answer for the enroll endpoint.
fn enroll_error(status: StatusCode, error: &str) -> Response {
    (status, Json(json!({ "error": error }))).into_response()
}

/// Decodes a base64url public key.
fn decode_key(raw: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(raw.trim())
        .ok()
}

/// `POST /api/node/v1/enroll`: redeem an enrollment link. Answers
/// `{cluster_id, node_id}`; 403 `transport_insecure` when the connection
/// fails the transport policy, 403 `enrollment_refused` for any link that
/// does not redeem.
pub async fn post_enroll(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let (mut parts, body) = request.into_parts();
    // The extractor (not a bare extension read) so a missing peer stays
    // `None` and tests' `MockConnectInfo` is honored.
    let peer = <ConnectInfo<SocketAddr> as axum::extract::FromRequestParts<Arc<AppState>>>::from_request_parts(&mut parts, &state)
        .await
        .ok()
        .map(|ConnectInfo(addr)| addr);
    let headers = std::mem::take(&mut parts.headers);
    let conn = ConnInfo {
        peer,
        local: parts
            .extensions
            .get::<LocalAddr>()
            .map(|LocalAddr(addr)| *addr),
        listener: parts
            .extensions
            .get::<ListenerKind>()
            .copied()
            .unwrap_or(ListenerKind::Plain),
    };
    let decision = {
        let cfg = state.config.read().await;
        classify_transport(&conn, &headers, &TransportConfig::from_config(&cfg))
    };
    if let TransportDecision::Refused(reason) = decision {
        tracing::warn!(
            peer = ?conn.peer,
            reason = reason.as_str(),
            "node enroll refused: insecure transport"
        );
        return refused_response(reason);
    }
    let json_body = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
        });
    if !json_body {
        return enroll_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "expected application/json",
        );
    }
    // Enroll bodies are small; refuse anything larger without buffering it.
    let Ok(body) = axum::body::to_bytes(body, MAX_ENROLL_BODY_BYTES).await else {
        return enroll_error(StatusCode::PAYLOAD_TOO_LARGE, "enroll request too large");
    };
    let Ok(body) = serde_json::from_slice::<EnrollBody>(&body) else {
        return enroll_error(StatusCode::BAD_REQUEST, "invalid enroll request");
    };
    let (Some(sign_public_key), Some(box_public_key)) =
        (decode_key(&body.sign_pk), decode_key(&body.box_pk))
    else {
        return enroll_error(
            StatusCode::BAD_REQUEST,
            "sign_pk and box_pk must be base64url",
        );
    };
    let request = RedeemRequest {
        link_id: body.link_id.trim().to_string(),
        secret: EnrollmentSecret::from_text(body.secret.trim().to_string()),
        sign_public_key,
        box_public_key,
        software_version: body.software_version,
    };
    let library = state.library_snapshot().await;
    match redeem_enrollment_link(&library, &request).await {
        Ok(enrolled) => Json(json!({
            "cluster_id": enrolled.cluster_id,
            "node_id": enrolled.node_id,
        }))
        .into_response(),
        Err(LibraryError::Forbidden(_)) => {
            enroll_error(StatusCode::FORBIDDEN, "enrollment_refused")
        }
        Err(LibraryError::Invalid(detail)) => enroll_error(StatusCode::BAD_REQUEST, &detail),
        Err(err) => {
            tracing::warn!(error = %err, "node enroll failed");
            enroll_error(StatusCode::SERVICE_UNAVAILABLE, "enrollment_unavailable")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::{delete, get};
    use bookclerk_config::Config;
    use bookclerk_library::control_plane::bootstrap_control_plane;
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use crate::auth::UserCaller;

    /// The enroll route as the daemon mounts it, seen from `peer` on a
    /// listener bound to `local`.
    fn enroll_route(state: &Arc<crate::api::AppState>, peer: &str, local: &str) -> axum::Router {
        let peer: std::net::SocketAddr = peer.parse().unwrap();
        let local: std::net::SocketAddr = local.parse().unwrap();
        axum::Router::new()
            .route(
                "/api/node/v1/enroll",
                axum::routing::post(super::post_enroll),
            )
            .layer(axum::Extension(super::transport::LocalAddr(local)))
            .layer(axum::extract::connect_info::MockConnectInfo(peer))
            .with_state(Arc::clone(state))
    }

    fn key(byte: u8) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([byte; 32])
    }

    #[tokio::test]
    async fn enroll_redeems_over_loopback_and_refuses_plain_lan() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let files = tempfile::tempdir().unwrap();
        let (state, store) = test_state(&files).await;
        let member = store
            .create_user(bookclerk_library::UserRole::Member, Some("Riley"), None)
            .await
            .unwrap();
        let minted = bookclerk_library::control_plane::mint_enrollment_link(
            &store,
            &bookclerk_library::control_plane::EnrollmentActor::User { user_id: member.id },
            member.id,
            "",
            None,
        )
        .await
        .unwrap();
        let body = json!({
            "link_id": minted.link.link_id,
            "secret": minted.secret.expose(),
            "sign_pk": key(4),
            "box_pk": key(5),
            "software_version": "1.0.0",
        });

        // A LAN peer over plain HTTP is refused before the link is read.
        let lan = enroll_route(&state, "192.168.1.20:5000", "0.0.0.0:8787");
        let (status, refused) = send(&lan, "POST", "/api/node/v1/enroll", body.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
        assert_eq!(refused["error"], "transport_insecure");
        assert_eq!(refused["reason"], "plain_http");
        // Spoofed forwarding headers from that peer change nothing.
        let request = Request::builder()
            .method("POST")
            .uri("/api/node/v1/enroll")
            .header("content-type", "application/json")
            .header("x-forwarded-proto", "https")
            .header("x-forwarded-for", "127.0.0.1")
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = lan.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let local = enroll_route(&state, "127.0.0.1:5000", "127.0.0.1:8787");
        let (status, enrolled) = send(&local, "POST", "/api/node/v1/enroll", body.clone()).await;
        assert_eq!(status, StatusCode::OK, "{enrolled}");
        let node_id = enrolled["node_id"].as_str().unwrap();
        let node = bookclerk_library::control_plane::load_node(&store, node_id)
            .await
            .unwrap();
        assert_eq!(node.kind, bookclerk_library::control_plane::NodeKind::Host);
        assert_eq!(node.owner_user_id, Some(member.id));

        // Single use: different keys are refused with one generic answer.
        let mut reuse = body.clone();
        reuse["sign_pk"] = json!(key(9));
        let (status, again) = send(&local, "POST", "/api/node/v1/enroll", reuse).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{again}");
        assert_eq!(again["error"], "enrollment_refused");
        let (status, _) = send(
            &local,
            "POST",
            "/api/node/v1/enroll",
            json!({ "link_id": "x", "secret": "y", "sign_pk": "z", "box_pk": "w", "extra": 1 }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    async fn send(app: &axum::Router, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// Invite routes for one caller, as the daemon mounts them.
    fn invite_routes(state: &Arc<crate::api::AppState>, caller: UserCaller) -> axum::Router {
        axum::Router::new()
            .route(
                "/api/config/node-invites",
                get(super::get_node_invites).post(super::post_node_invite),
            )
            .route(
                "/api/config/node-invites/{id}",
                delete(super::delete_node_invite),
            )
            .route(
                "/api/config/node-credentials/{node_id}",
                delete(super::delete_node_credential),
            )
            .layer(axum::Extension(caller))
            .with_state(Arc::clone(state))
    }

    /// A bootstrapped single-node cluster and its daemon state.
    async fn test_state(
        files: &tempfile::TempDir,
    ) -> (Arc<crate::api::AppState>, bookclerk_library::LibraryStore) {
        let files_path = files.path();
        std::fs::write(files_path.join("config.toml"), "").unwrap();
        let db = bookclerk_plugin_database_sqlite::open(&files_path.join("library.db"))
            .await
            .unwrap();
        bookclerk_library::apply_host_schema(&db).await.unwrap();
        let store = bookclerk_library::LibraryStore::from_connection(db);
        bootstrap_control_plane(
            &store,
            files_path,
            &files_path.join("cache"),
            None,
            &Config::default(),
        )
        .await
        .unwrap();
        let config = Config::load(
            Some(files_path.to_path_buf()),
            Some(files_path.join("config.toml")),
        )
        .unwrap();
        let state = Arc::new(crate::config_authority::control_plane_test_state(
            store.clone(),
            config,
        ));
        (state, store)
    }

    #[tokio::test]
    async fn a_member_mints_invites_for_themselves_only() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let files = tempfile::tempdir().unwrap();
        let (state, store) = test_state(&files).await;
        let member = store
            .create_user(bookclerk_library::UserRole::Member, Some("Riley"), None)
            .await
            .unwrap();
        let other = store
            .create_user(bookclerk_library::UserRole::Member, Some("Sam"), None)
            .await
            .unwrap();
        let as_member = invite_routes(&state, UserCaller::User { user_id: member.id });
        let operator = invite_routes(&state, UserCaller::Operator);

        let (status, minted) = send(
            &as_member,
            "POST",
            "/api/config/node-invites",
            json!({ "label": "desk" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{minted}");
        assert_eq!(minted["link"]["for_user_id"], member.id);
        let url = minted["url"].as_str().unwrap().to_string();
        let link_id = minted["link"]["link_id"].as_str().unwrap().to_string();
        assert!(
            url.contains(&format!("/enroll#bcn1.{link_id}.")),
            "secret travels in the fragment: {url}"
        );
        assert!(minted["link"].get("token_hash").is_none());

        let (status, refused) = send(
            &as_member,
            "POST",
            "/api/config/node-invites",
            json!({ "user_id": other.id }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
        let (status, _) = send(&operator, "POST", "/api/config/node-invites", json!({})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, for_other) = send(
            &operator,
            "POST",
            "/api/config/node-invites",
            json!({ "user_id": other.id, "ttl_secs": 3600 }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{for_other}");

        // Each user lists only their own links; listings never carry the secret.
        let secret = &url[url.rfind('.').unwrap() + 1..];
        let (_, listed) = send(&as_member, "GET", "/api/config/node-invites", Value::Null).await;
        assert_eq!(listed["links"].as_array().unwrap().len(), 1);
        assert!(!listed.to_string().contains(secret));
        let (_, all) = send(&operator, "GET", "/api/config/node-invites", Value::Null).await;
        assert_eq!(all["links"].as_array().unwrap().len(), 2);

        let other_id = for_other["link"]["link_id"].as_str().unwrap();
        let (status, _) = send(
            &as_member,
            "DELETE",
            &format!("/api/config/node-invites/{other_id}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, revoked) = send(
            &as_member,
            "DELETE",
            &format!("/api/config/node-invites/{link_id}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{revoked}");
        assert_eq!(revoked["link"]["state"], "revoked");
    }

    #[tokio::test]
    async fn a_user_revokes_only_the_hosts_they_own() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let files = tempfile::tempdir().unwrap();
        let (state, store) = test_state(&files).await;
        let member = store
            .create_user(bookclerk_library::UserRole::Member, Some("Riley"), None)
            .await
            .unwrap();
        let other = store
            .create_user(bookclerk_library::UserRole::Member, Some("Sam"), None)
            .await
            .unwrap();
        let minted = bookclerk_library::control_plane::mint_enrollment_link(
            &store,
            &bookclerk_library::control_plane::EnrollmentActor::User { user_id: member.id },
            member.id,
            "",
            None,
        )
        .await
        .unwrap();
        let local = enroll_route(&state, "127.0.0.1:5000", "127.0.0.1:8787");
        let (status, enrolled) = send(
            &local,
            "POST",
            "/api/node/v1/enroll",
            json!({
                "link_id": minted.link.link_id,
                "secret": minted.secret.expose(),
                "sign_pk": key(4),
                "box_pk": key(5),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{enrolled}");
        let uri = format!(
            "/api/config/node-credentials/{}",
            enrolled["node_id"].as_str().unwrap()
        );

        let as_other = invite_routes(&state, UserCaller::User { user_id: other.id });
        let (status, _) = send(&as_other, "DELETE", &uri, Value::Null).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let as_member = invite_routes(&state, UserCaller::User { user_id: member.id });
        let (status, revoked) = send(&as_member, "DELETE", &uri, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{revoked}");
        assert!(revoked["credential"]["revoked_at"].is_string(), "{revoked}");
        assert!(revoked["credential"].get("sign_public_key").is_none());
    }

    #[tokio::test]
    async fn only_the_operator_or_an_owner_toggles_host_enrollment() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let files = tempfile::tempdir().unwrap();
        let (state, store) = test_state(&files).await;
        let owner = store
            .create_user(bookclerk_library::UserRole::Owner, Some("Avery"), None)
            .await
            .unwrap();
        let member = store
            .create_user(bookclerk_library::UserRole::Member, Some("Riley"), None)
            .await
            .unwrap();
        let routes = |caller| {
            axum::Router::new()
                .route(
                    "/api/config/host-enrollment",
                    get(super::get_host_enrollment).put(super::put_host_enrollment),
                )
                .layer(axum::Extension(caller))
                .with_state(Arc::clone(&state))
        };
        let as_owner = routes(UserCaller::User { user_id: owner.id });
        let as_member = routes(UserCaller::User { user_id: member.id });
        let operator = routes(UserCaller::Operator);
        let uri = "/api/config/host-enrollment";

        let (status, read) = send(&as_member, "GET", uri, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{read}");
        assert_eq!(read, json!({ "enabled": true, "revision": null }));

        let (status, refused) = send(&as_member, "PUT", uri, json!({ "enabled": false })).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");

        let (status, off) = send(&as_owner, "PUT", uri, json!({ "enabled": false })).await;
        assert_eq!(status, StatusCode::OK, "{off}");
        assert_eq!(off["enabled"], false);
        let (_, read) = send(&as_member, "GET", uri, Value::Null).await;
        assert_eq!(read["enabled"], false);

        // The switch now refuses new links.
        let invites = invite_routes(&state, UserCaller::User { user_id: member.id });
        let (status, _) = send(&invites, "POST", "/api/config/node-invites", json!({})).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let (status, on) = send(&operator, "PUT", uri, json!({ "enabled": true })).await;
        assert_eq!(status, StatusCode::OK, "{on}");
        assert_eq!(on["enabled"], true);
    }
}
