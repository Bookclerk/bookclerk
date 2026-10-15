//! Host-node enrollment over HTTP.
//!
//! `/api/config/node-invites` lets a signed-in user mint, list, and revoke
//! enrollment links (the library decides who may act for whom). A minted
//! link's secret appears once, in the `url` fragment of the mint response;
//! it is never logged.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::{Extension, Json};
use bookclerk_library::control_plane::{
    list_enrollment_links, mint_enrollment_link, revoke_enrollment_link, EnrollmentActor,
    EnrollmentLink,
};
use serde::Deserialize;
use serde_json::json;

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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::{delete, get};
    use bookclerk_config::{Config, EventsConfig};
    use bookclerk_library::control_plane::bootstrap_control_plane;
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use crate::auth::UserCaller;

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
            &EventsConfig::default(),
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
}
