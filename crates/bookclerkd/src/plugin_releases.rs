//! `/api/config/plugin-releases`: the daemon's view of cluster releases.
//!
//! `GET` lists releases with the active pointer; `POST` publishes from a
//! registry coordinate or an operator-placed package directory under
//! `plugin-packages/`; `PUT …/{key}/active` moves the pointer (activate,
//! update, rollback); `DELETE …/{key}/active` clears it and sets every
//! deployment absent; `POST …/{key}/{version}/approve` records consent.
//! Hosts apply every change when their `host.<host_id>` queue wakes them.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use bookclerk_library::control_plane::{
    activate_release, approve_release_grant, clear_active_release, delete_plugin_placement,
    list_deployments_for_instance, list_plugin_instances_for_key, list_release_pointers,
    list_releases, load_or_create_host_identity, load_release, replace_plugin_deployment,
    ConfigActor, PluginRelease, DESIRED_ABSENT,
};
use bookclerk_plugin_catalog::{
    safe_join, BookclerkPackageManifest, Installer, PackageCoordinate, TrustPolicy,
};
use bookclerk_plugin_host::{
    host_artifact, install_release, release_consent_request, InstallRequest, AUTHORIZED_PACKAGE_DIR,
};
use serde::Deserialize;
use serde_json::json;

use crate::api::AppState;

/// Operator actor recorded on release writes from the daemon.
fn operator_actor() -> ConfigActor {
    ConfigActor::Operator {
        id: "operator".into(),
    }
}

/// JSON error response.
fn error(status: StatusCode, message: impl std::fmt::Display) -> Response {
    (status, Json(json!({ "error": message.to_string() }))).into_response()
}

/// Maps a library error onto an HTTP status.
fn library_error(err: bookclerk_library::LibraryError) -> Response {
    let message = err.to_string();
    let status = if message.contains("unauthorized configuration write") {
        StatusCode::FORBIDDEN
    } else if message.contains("not found") || message.contains("NotFound") {
        StatusCode::NOT_FOUND
    } else if message.contains("already published with different content") {
        StatusCode::CONFLICT
    } else if message.contains("invalid") {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    error(status, message)
}

/// Maps an install failure onto an HTTP status: an alias or content clash
/// with an existing release is a conflict, anything else a bad request.
fn publish_error(err: bookclerk_plugin_host::PluginError) -> Response {
    let message = err.to_string();
    let status = if message.contains("already the active release")
        || message.contains("published under several plugin keys")
        || message.contains("already published with different content")
    {
        StatusCode::CONFLICT
    } else {
        StatusCode::BAD_REQUEST
    };
    error(status, message)
}

/// JSON projection of one release.
fn release_json(release: &PluginRelease, active: bool) -> serde_json::Value {
    json!({
        "plugin_key": release.plugin_key,
        "id": release.manifest_id,
        "version": release.version,
        "active": active,
        "approved": !release.grant_json.is_empty(),
        "publisher_verified": release.publisher_verified,
        "coordinate": release.coordinate,
        "published_at": release.published_at,
        "published_by": release.published_by,
        "host_payload_root_sha256": host_artifact(release).ok().map(|a| a.payload_root_sha256.clone()),
        "artifacts": release.artifacts.iter().map(|a| json!({
            "target": a.target,
            "url": a.url,
            "archive_sha256": a.archive_sha256,
            "payload_root_sha256": a.payload_root_sha256,
        })).collect::<Vec<_>>(),
    })
}

/// `GET /api/config/plugin-releases`.
pub async fn get_plugin_releases(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, Response> {
    let library = state.library_snapshot().await;
    let pointers = list_release_pointers(&library)
        .await
        .map_err(library_error)?;
    let releases = list_releases(&library, None).await.map_err(library_error)?;
    let rows: Vec<_> = releases
        .iter()
        .map(|release| {
            let active = pointers
                .iter()
                .any(|p| p.plugin_key == release.plugin_key && p.version == release.version);
            release_json(release, active)
        })
        .collect();
    Ok(Json(json!({ "releases": rows })))
}

/// `POST /api/config/plugin-releases` body: exactly one of `coordinate` or
/// `package` (a directory name under `plugin-packages/`).
#[derive(Debug, Deserialize)]
pub(crate) struct PostPluginRelease {
    /// Registry coordinate (`cargo:…@v`, `registry:…#name@v`).
    #[serde(default)]
    coordinate: Option<String>,
    /// Operator-placed package directory under `plugin-packages/`.
    #[serde(default)]
    package: Option<String>,
    /// Make the new release active even when another is.
    #[serde(default)]
    activate: bool,
    /// Record the requested consent as approved.
    #[serde(default)]
    approve: bool,
    /// Accept a publisher without independent authenticity.
    #[serde(default)]
    allow_unverified_publisher: bool,
}

/// `POST /api/config/plugin-releases`: publish (and maybe activate) a release.
pub async fn post_plugin_release(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PostPluginRelease>,
) -> Result<Json<serde_json::Value>, Response> {
    let config = state.config.read().await.clone();
    let files_dir = config.paths().files_dir.clone();
    let trust = TrustPolicy {
        allow_unverified_publisher: body.allow_unverified_publisher
            || config.plugins.allow_unverified_publisher,
        ..TrustPolicy::default()
    };
    trust.check_unverified_publisher_allowed().map_err(|err| {
        error(
            StatusCode::FORBIDDEN,
            format!("{err} (API: set `allow_unverified_publisher: true` in the request body)"),
        )
    })?;
    let host_id = load_or_create_host_identity(&files_dir)
        .map_err(library_error)?
        .host_id;
    let library = state.library_snapshot().await;
    let (package, coordinate, archive) = match (body.coordinate.as_deref(), body.package.as_deref())
    {
        (Some(raw), None) => {
            let coordinate =
                PackageCoordinate::parse(raw).map_err(|err| error(StatusCode::BAD_REQUEST, err))?;
            let lookup = coordinate.clone();
            let package = tokio::task::spawn_blocking(move || {
                bookclerk_plugin_catalog::fetch_manifest_for_coordinate(&lookup, &[])
            })
            .await
            .map_err(|err| error(StatusCode::INTERNAL_SERVER_ERROR, err))?
            .map_err(|err| error(StatusCode::BAD_GATEWAY, err))?;
            (package, coordinate, None)
        }
        (None, Some(name)) => {
            let dir = safe_join(
                &files_dir.join(AUTHORIZED_PACKAGE_DIR),
                std::path::Path::new(name),
            )
            .map_err(|err| error(StatusCode::BAD_REQUEST, err))?;
            let text = std::fs::read_to_string(dir.join("package.json"))
                .map_err(|err| error(StatusCode::NOT_FOUND, format!("package.json: {err}")))?;
            let package = BookclerkPackageManifest::from_json(&text)
                .map_err(|err| error(StatusCode::BAD_REQUEST, err))?;
            let archive = dir.join("archive.tar.gz");
            if !archive.is_file() {
                return Err(error(StatusCode::NOT_FOUND, "archive.tar.gz is missing"));
            }
            let coordinate = Installer::local_archive_coordinate(&archive, &package);
            (package, coordinate, Some(archive))
        }
        _ => {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "pass exactly one of `coordinate` or `package`",
            ))
        }
    };
    let installed = install_release(
        &library,
        &operator_actor(),
        &files_dir,
        &host_id,
        InstallRequest {
            package,
            coordinate,
            archive,
            target: None,
            offline: false,
            activate: body.activate,
            approve: body.approve,
        },
    )
    .await
    .map_err(publish_error)?;
    Ok(Json(json!({
        "created": installed.created,
        "plugin_instance_id": installed.instance.id.to_string(),
        "release": release_json(&installed.release, installed.active),
    })))
}

/// `PUT /api/config/plugin-releases/{key}/active` body.
#[derive(Debug, Deserialize)]
pub(crate) struct PutActiveRelease {
    /// Published version to activate.
    version: String,
}

/// `PUT /api/config/plugin-releases/{key}/active`: activate, update, or
/// roll back.
pub async fn put_active_release(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Json(body): Json<PutActiveRelease>,
) -> Result<Json<serde_json::Value>, Response> {
    let config = state.config.read().await.clone();
    let host_id = load_or_create_host_identity(&config.paths().files_dir)
        .map_err(library_error)?
        .host_id;
    let library = state.library_snapshot().await;
    let (pointer, _) = activate_release(
        &library,
        &operator_actor(),
        &key,
        body.version.trim(),
        &host_id,
    )
    .await
    .map_err(library_error)?;
    Ok(Json(json!({
        "plugin_key": pointer.plugin_key,
        "version": pointer.version,
        "revision": pointer.revision,
    })))
}

/// `DELETE /api/config/plugin-releases/{key}/active`: remove the plugin from
/// the cluster (pointer cleared, every deployment absent).
pub async fn delete_active_release(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Result<Json<serde_json::Value>, Response> {
    let library = state.library_snapshot().await;
    let mut disabled = 0;
    for instance in list_plugin_instances_for_key(&library, &key)
        .await
        .map_err(library_error)?
    {
        // A placed instance would be redeployed by the scheduler; removing
        // the plugin removes its placement first.
        delete_plugin_placement(&library, &operator_actor(), &instance.id)
            .await
            .map_err(library_error)?;
        for deployment in list_deployments_for_instance(&library, &instance.id)
            .await
            .map_err(library_error)?
        {
            if deployment.desired == DESIRED_ABSENT {
                continue;
            }
            replace_plugin_deployment(
                &library,
                &operator_actor(),
                &deployment.deployment_id,
                deployment.revision,
                DESIRED_ABSENT,
                &uuid::Uuid::new_v4().to_string(),
            )
            .await
            .map_err(library_error)?;
            disabled += 1;
        }
    }
    let cleared = clear_active_release(&library, &operator_actor(), &key)
        .await
        .map_err(library_error)?;
    Ok(Json(json!({
        "plugin_key": key,
        "cleared": cleared,
        "deployments_disabled": disabled,
    })))
}

/// `POST /api/config/plugin-releases/{key}/{version}/approve`: record the
/// release's requested consent as approved for every host.
pub async fn approve_plugin_release(
    State(state): State<Arc<AppState>>,
    Path((key, version)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, Response> {
    let library = state.library_snapshot().await;
    let release = load_release(&library, &key, &version)
        .await
        .map_err(library_error)?
        .ok_or_else(|| error(StatusCode::NOT_FOUND, format!("release {key} {version}")))?;
    let grant =
        release_consent_request(&release).map_err(|err| error(StatusCode::BAD_REQUEST, err))?;
    let grant_json = serde_json::to_string(&grant)
        .map_err(|err| error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    let release = approve_release_grant(&library, &operator_actor(), &key, &version, &grant_json)
        .await
        .map_err(library_error)?;
    Ok(Json(json!({
        "plugin_key": release.plugin_key,
        "version": release.version,
        "approved": true,
        "grant": grant,
    })))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::{get, post, put};
    use bookclerk_config::{Config, EventsConfig};
    use bookclerk_library::control_plane::{bootstrap_control_plane, load_active_release};
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    /// Writes `plugin-packages/<dir>/{archive.tar.gz,package.json}` with the
    /// GraphicAudio `plugin.toml` at `version` and a placeholder executable
    /// (publishing verifies the tree; it never runs the guest).
    fn stage_package(files: &Path, dir: &str, version: &str) {
        let toml = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../bookclerk-plugins/optional/source-graphicaudio/plugin.toml");
        let text = std::fs::read_to_string(toml).unwrap().replacen(
            "version = \"0.1.0\"",
            &format!("version = \"{version}\""),
            1,
        );
        let staging = files.join(format!("{dir}-stage"));
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("plugin.toml"), text).unwrap();
        let exe = "bookclerk-plugin-source-graphicaudio";
        std::fs::write(staging.join(exe), b"#!/bin/sh\n").unwrap();
        let package = files
            .join(bookclerk_plugin_host::AUTHORIZED_PACKAGE_DIR)
            .join(dir);
        std::fs::create_dir_all(&package).unwrap();
        let archive = package.join("archive.tar.gz");
        let status = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&staging)
            .args(["plugin.toml", exe])
            .status()
            .expect("tar");
        assert!(status.success());
        let manifest = json!({
            "schema_version": 1,
            "api_version": 1,
            "kind": "source",
            "id": "graphicaudio",
            "sandbox": { "network": "outbound" },
            "artifacts": [{
                "target": bookclerk_plugin_host::host_bookclerk_target(),
                "url": format!("file://{}", archive.canonicalize().unwrap().display()),
                "archive_sha256": "ab".repeat(32),
                "executable": exe
            }]
        });
        std::fs::write(
            package.join("package.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
    }

    /// Sends one JSON request and returns the status and parsed body.
    async fn send(
        app: &axum::Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(
                body.map(|b| b.to_string()).unwrap_or_else(|| "{}".into()),
            ))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    /// Percent-encodes a plugin key for one path segment.
    fn segment(key: &str) -> String {
        url::form_urlencoded::byte_serialize(key.as_bytes())
            .collect::<String>()
            .replace('+', "%20")
    }

    #[tokio::test]
    async fn release_routes_publish_activate_approve_and_clear() {
        let files = tempfile::tempdir().unwrap();
        let files_path = files.path();
        std::fs::write(files_path.join("config.toml"), "").unwrap();
        stage_package(files_path, "ga1", "0.1.0");
        stage_package(files_path, "ga2", "0.2.0");

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
        let app = axum::Router::new()
            .route(
                "/api/config/plugin-releases",
                get(super::get_plugin_releases).post(super::post_plugin_release),
            )
            .route(
                "/api/config/plugin-releases/{key}/active",
                put(super::put_active_release).delete(super::delete_active_release),
            )
            .route(
                "/api/config/plugin-releases/{key}/{version}/approve",
                post(super::approve_plugin_release),
            )
            .with_state(state);

        // A first publish becomes the active release; approval is recorded.
        let (status, first) = send(
            &app,
            "POST",
            "/api/config/plugin-releases",
            Some(json!({ "package": "ga1", "approve": true, "allow_unverified_publisher": true })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(first["created"], true);
        assert_eq!(first["release"]["active"], true);
        assert_eq!(first["release"]["approved"], true);
        assert_eq!(first["release"]["version"], "0.1.0");
        let key = first["release"]["plugin_key"].as_str().unwrap().to_owned();

        // Publishing the same package again is idempotent.
        let (status, again) = send(
            &app,
            "POST",
            "/api/config/plugin-releases",
            Some(json!({ "package": "ga1", "allow_unverified_publisher": true })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{again}");
        assert_eq!(again["created"], false);

        // A newer local archive of the same alias lands under the same key,
        // carries the approval forward, and does not displace the active
        // release without `activate`.
        let (status, second) = send(
            &app,
            "POST",
            "/api/config/plugin-releases",
            Some(json!({ "package": "ga2", "allow_unverified_publisher": true })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{second}");
        assert_eq!(second["release"]["plugin_key"], key.as_str());
        assert_eq!(second["release"]["active"], false);
        assert_eq!(second["release"]["approved"], true);

        let (status, listed) = send(&app, "GET", "/api/config/plugin-releases", None).await;
        assert_eq!(status, StatusCode::OK);
        let rows = listed["releases"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.iter().filter(|r| r["active"] == true).count(),
            1,
            "{listed}"
        );

        // Moving the pointer updates; an unknown version is refused.
        let active_uri = format!("/api/config/plugin-releases/{}/active", segment(&key));
        let (status, moved) = send(
            &app,
            "PUT",
            &active_uri,
            Some(json!({ "version": "0.2.0" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{moved}");
        assert_eq!(moved["version"], "0.2.0");
        assert_eq!(
            load_active_release(&store, &key)
                .await
                .unwrap()
                .unwrap()
                .version,
            "0.2.0"
        );
        let (status, _) = send(
            &app,
            "PUT",
            &active_uri,
            Some(json!({ "version": "9.9.9" })),
        )
        .await;
        assert!(status.is_client_error(), "{status}");

        let approve_uri = format!(
            "/api/config/plugin-releases/{}/0.1.0/approve",
            segment(&key)
        );
        let (status, approved) = send(&app, "POST", &approve_uri, None).await;
        assert_eq!(status, StatusCode::OK, "{approved}");
        assert_eq!(approved["approved"], true);

        let (status, cleared) = send(&app, "DELETE", &active_uri, None).await;
        assert_eq!(status, StatusCode::OK, "{cleared}");
        assert_eq!(cleared["cleared"], true);
        assert!(load_active_release(&store, &key).await.unwrap().is_none());

        // Without the operator override the publisher is refused.
        let (status, refused) = send(
            &app,
            "POST",
            "/api/config/plugin-releases",
            Some(json!({ "package": "ga1" })),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");

        // The package name is a directory under plugin-packages/, nothing
        // else; and exactly one source must be named.
        for mut body in [
            json!({ "package": "../ga1" }),
            json!({ "package": "/etc" }),
            json!({}),
            json!({ "package": "ga1", "coordinate": "cargo:x@1.0.0" }),
        ] {
            body["allow_unverified_publisher"] = json!(true);
            let (status, value) = send(
                &app,
                "POST",
                "/api/config/plugin-releases",
                Some(body.clone()),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {value}");
        }
    }
}
