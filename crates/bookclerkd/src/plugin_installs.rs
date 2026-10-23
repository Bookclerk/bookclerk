//! `/api/config/plugins`: plugins installed in the cluster.
//!
//! `GET` lists installed plugins with their versions; `POST` installs a
//! version from a registry coordinate or an operator-placed package
//! directory under `plugin-packages/`. `GET …/{key}` reads one plugin and
//! its pinned settings schema; `DELETE …/{key}` uninstalls it (clears the
//! active version and undeploys it everywhere). `PUT …/{key}/active` moves
//! the active version (update or rollback), and
//! `POST …/{key}/versions/{version}/approve` records consent. Nodes apply
//! every change when their `node.<node_id>` queue wakes them.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use bookclerk_library::control_plane::{
    activate_version, approve_version_grant, clear_install, list_installs,
    list_plugin_instances_for_key, list_versions, load_install, load_or_create_node_identity,
    load_plugin_schema, load_version, undeploy_instance, ConfigActor, PluginInstall, PluginVersion,
};
use bookclerk_plugin_catalog::{
    safe_join, BookclerkPackageManifest, Installer, PackageCoordinate, TrustPolicy,
};
use bookclerk_plugin_host::{
    host_artifact, install_version, version_consent_request, InstallRequest, PluginError,
    AUTHORIZED_PACKAGE_DIR,
};
use serde::Deserialize;
use serde_json::json;

use crate::api::AppState;
use crate::config_authority::events_error as library_error;

/// Operator actor recorded on install writes from the daemon.
fn operator_actor() -> ConfigActor {
    ConfigActor::Operator {
        id: "operator".into(),
    }
}

/// JSON error response.
fn error(status: StatusCode, message: impl std::fmt::Display) -> Response {
    (status, Json(json!({ "error": message.to_string() }))).into_response()
}

/// Maps an install failure onto an HTTP status by its kind: a clash with
/// an existing version or alias is a conflict, anything else a bad request.
fn install_error(err: PluginError) -> Response {
    let status = match &err {
        PluginError::Conflict(_) => StatusCode::CONFLICT,
        PluginError::NotFound(_) => StatusCode::NOT_FOUND,
        PluginError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::BAD_REQUEST,
    };
    error(status, err)
}

/// JSON projection of one installed version.
fn version_json(version: &PluginVersion, active: bool) -> serde_json::Value {
    json!({
        "plugin_key": version.plugin_key,
        "id": version.manifest_id,
        "version": version.version,
        "active": active,
        "approved": !version.grant_json.is_empty(),
        "publisher_verified": version.publisher_verified,
        "coordinate": version.coordinate,
        "installed_at": version.installed_at,
        "installed_by": version.installed_by,
        "host_payload_root_sha256": host_artifact(version).ok().map(|a| a.payload_root_sha256.clone()),
        "artifacts": version.artifacts.iter().map(|a| json!({
            "target": a.target,
            "url": a.url,
            "archive_sha256": a.archive_sha256,
            "payload_root_sha256": a.payload_root_sha256,
        })).collect::<Vec<_>>(),
    })
}

/// JSON projection of one installed plugin: its active version and every
/// installed version, newest first.
fn plugin_json(
    plugin_key: &str,
    versions: &[&PluginVersion],
    install: Option<&PluginInstall>,
) -> serde_json::Value {
    let active = install.map(|i| i.version.as_str());
    let alias = versions
        .iter()
        .find(|v| Some(v.version.as_str()) == active)
        .or_else(|| versions.first())
        .map(|v| v.manifest_id.clone());
    json!({
        "plugin_key": plugin_key,
        "id": alias,
        "active_version": active,
        "revision": install.map(|i| i.revision),
        "versions": versions
            .iter()
            .map(|v| version_json(v, Some(v.version.as_str()) == active))
            .collect::<Vec<_>>(),
    })
}

/// `GET /api/config/plugins`: every installed plugin.
#[utoipa::path(
    get,
    path = "/api/config/plugins",
    responses(
        (status = 200, description = "Every installed plugin.", body = serde_json::Value),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
    ),
)]
pub async fn get_plugins(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, Response> {
    let library = state.library_snapshot().await;
    let installs = list_installs(&library).await.map_err(library_error)?;
    let versions = list_versions(&library, None).await.map_err(library_error)?;
    let mut keys: Vec<&str> = versions.iter().map(|v| v.plugin_key.as_str()).collect();
    keys.sort_unstable();
    keys.dedup();
    let plugins: Vec<_> = keys
        .into_iter()
        .map(|key| {
            let of_key: Vec<&PluginVersion> =
                versions.iter().filter(|v| v.plugin_key == key).collect();
            plugin_json(key, &of_key, installs.iter().find(|i| i.plugin_key == key))
        })
        .collect();
    Ok(Json(json!({ "plugins": plugins })))
}

/// `GET /api/config/plugins/{key}`: one installed plugin with its pinned
/// settings schema.
#[utoipa::path(
    get,
    path = "/api/config/plugins/{key}",
    params(("key" = String, Path, description = "Plugin key.")),
    responses(
        (status = 200, description = "The plugin with its pinned versions.", body = serde_json::Value),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
        (status = 404, description = "No such plugin."),
    ),
)]
pub async fn get_plugin(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Result<Json<serde_json::Value>, Response> {
    let library = state.library_snapshot().await;
    let versions = list_versions(&library, Some(&key))
        .await
        .map_err(library_error)?;
    if versions.is_empty() {
        return Err(error(
            StatusCode::NOT_FOUND,
            format!("plugin {key} is not installed"),
        ));
    }
    let install = load_install(&library, &key).await.map_err(library_error)?;
    let of_key: Vec<&PluginVersion> = versions.iter().collect();
    let mut body = plugin_json(&key, &of_key, install.as_ref());
    let schema = load_plugin_schema(&library, &key)
        .await
        .map_err(library_error)?;
    body["schema"] = schema.map_or(serde_json::Value::Null, |pinned| {
        json!({
            "manifest_id": pinned.manifest_id,
            "schema_sha256": pinned.schema_sha256,
            "package_version": pinned.package_version,
            "recorded_at": pinned.recorded_at,
            "recorded_by_node": pinned.recorded_by_node,
            "settings": pinned.schema.settings,
        })
    });
    Ok(Json(body))
}

/// `POST /api/config/plugins` body: exactly one of `coordinate` or
/// `package` (a directory name under `plugin-packages/`).
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(crate) struct PostPluginInstall {
    /// Registry coordinate (`cargo:…@v`, `registry:…#name@v`).
    #[serde(default)]
    coordinate: Option<String>,
    /// Operator-placed package directory under `plugin-packages/`.
    #[serde(default)]
    package: Option<String>,
    /// Make the new version active even when another is.
    #[serde(default)]
    activate: bool,
    /// Record the requested consent as approved.
    #[serde(default)]
    approve: bool,
    /// Accept a publisher without independent authenticity.
    #[serde(default)]
    allow_unverified_publisher: bool,
}

/// `POST /api/config/plugins`: install (and maybe activate) a version.
#[utoipa::path(
    post,
    path = "/api/config/plugins",
    request_body = PostPluginInstall,
    responses(
        (status = 200, description = "The installed version.", body = serde_json::Value),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
        (status = 409, description = "The version is already installed."),
    ),
)]
pub async fn post_plugin_install(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PostPluginInstall>,
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
    let node_id = load_or_create_node_identity(&files_dir)
        .map_err(library_error)?
        .node_id;
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
    let installed = install_version(
        &library,
        &operator_actor(),
        &files_dir,
        &node_id,
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
    .map_err(install_error)?;
    Ok(Json(json!({
        "created": installed.created,
        "plugin_instance_id": installed.instance.id.to_string(),
        "version": version_json(&installed.version, installed.active),
    })))
}

/// `PUT /api/config/plugins/{key}/active` body.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(crate) struct PutActiveVersion {
    /// Installed version to activate.
    version: String,
}

/// `PUT /api/config/plugins/{key}/active`: activate, update, or roll back.
#[utoipa::path(
    put,
    path = "/api/config/plugins/{key}/active",
    params(("key" = String, Path, description = "Plugin key.")),
    request_body = PutActiveVersion,
    responses(
        (status = 200, description = "The active version after the change.", body = serde_json::Value),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
        (status = 404, description = "No such plugin."),
    ),
)]
pub async fn put_active_version(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Json(body): Json<PutActiveVersion>,
) -> Result<Json<serde_json::Value>, Response> {
    let config = state.config.read().await.clone();
    let node_id = load_or_create_node_identity(&config.paths().files_dir)
        .map_err(library_error)?
        .node_id;
    let library = state.library_snapshot().await;
    let (pointer, _) = activate_version(
        &library,
        &operator_actor(),
        &key,
        body.version.trim(),
        &node_id,
    )
    .await
    .map_err(library_error)?;
    Ok(Json(json!({
        "plugin_key": pointer.plugin_key,
        "version": pointer.version,
        "revision": pointer.revision,
    })))
}

/// `DELETE /api/config/plugins/{key}`: uninstall the plugin (active version
/// cleared, placements dropped, every deployment `absent`).
#[utoipa::path(
    delete,
    path = "/api/config/plugins/{key}",
    params(("key" = String, Path, description = "Plugin key.")),
    responses(
        (status = 200, description = "The plugin was uninstalled.", body = serde_json::Value),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
        (status = 404, description = "No such plugin."),
    ),
)]
pub async fn delete_plugin_install(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Result<Json<serde_json::Value>, Response> {
    let library = state.library_snapshot().await;
    let mut disabled = 0;
    // Every instance must be stopped before the active version goes; a
    // deployment that lost a race reports 409 and leaves the install alone.
    for instance in list_plugin_instances_for_key(&library, &key)
        .await
        .map_err(library_error)?
    {
        disabled += undeploy_instance(&library, &operator_actor(), &instance.id)
            .await
            .map_err(library_error)?;
    }
    let cleared = clear_install(&library, &operator_actor(), &key)
        .await
        .map_err(library_error)?;
    Ok(Json(json!({
        "plugin_key": key,
        "uninstalled": cleared,
        "deployments_undeployed": disabled,
    })))
}

/// `POST /api/config/plugins/{key}/versions/{version}/approve`: record the
/// version's requested consent as approved for every node.
#[utoipa::path(
    post,
    path = "/api/config/plugins/{key}/versions/{version}/approve",
    params(("key" = String, Path, description = "Plugin key."), ("version" = String, Path, description = "Semver version.")),
    responses(
        (status = 200, description = "The version's grant was approved.", body = serde_json::Value),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
        (status = 404, description = "No such plugin."),
    ),
)]
pub async fn approve_plugin_version(
    State(state): State<Arc<AppState>>,
    Path((key, version)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, Response> {
    let library = state.library_snapshot().await;
    let row = load_version(&library, &key, &version)
        .await
        .map_err(library_error)?
        .ok_or_else(|| error(StatusCode::NOT_FOUND, format!("version {key} {version}")))?;
    let grant = version_consent_request(&row).map_err(|err| error(StatusCode::BAD_REQUEST, err))?;
    let grant_json = serde_json::to_string(&grant)
        .map_err(|err| error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    let row = approve_version_grant(&library, &operator_actor(), &key, &version, &grant_json)
        .await
        .map_err(library_error)?;
    Ok(Json(json!({
        "plugin_key": row.plugin_key,
        "version": row.version,
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
    use bookclerk_config::Config;
    use bookclerk_library::control_plane::{bootstrap_control_plane, load_active_version};
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    /// Writes `plugin-packages/<dir>/{archive.tar.gz,package.json}` with the
    /// GraphicAudio `plugin.toml` at `version` and a placeholder executable
    /// (installing verifies the tree; it never runs the guest).
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
    async fn plugin_routes_install_activate_approve_and_uninstall() {
        let _dek = crate::auth::tests::process_dek_lock().await;
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
            &bookclerk_config::Config::default(),
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
                "/api/config/plugins",
                get(super::get_plugins).post(super::post_plugin_install),
            )
            .route(
                "/api/config/plugins/{key}",
                get(super::get_plugin).delete(super::delete_plugin_install),
            )
            .route(
                "/api/config/plugins/{key}/active",
                put(super::put_active_version),
            )
            .route(
                "/api/config/plugins/{key}/versions/{version}/approve",
                post(super::approve_plugin_version),
            )
            .with_state(state);

        // A first install becomes the active version; approval is recorded.
        let (status, first) = send(
            &app,
            "POST",
            "/api/config/plugins",
            Some(json!({ "package": "ga1", "approve": true, "allow_unverified_publisher": true })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(first["created"], true);
        assert_eq!(first["version"]["active"], true);
        assert_eq!(first["version"]["approved"], true);
        assert_eq!(first["version"]["version"], "0.1.0");
        let key = first["version"]["plugin_key"].as_str().unwrap().to_owned();

        // Installing the same package again is idempotent.
        let (status, again) = send(
            &app,
            "POST",
            "/api/config/plugins",
            Some(json!({ "package": "ga1", "allow_unverified_publisher": true })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{again}");
        assert_eq!(again["created"], false);

        // A newer local archive of the same alias lands under the same key,
        // carries the approval forward, and does not displace the active
        // version without `activate`.
        let (status, second) = send(
            &app,
            "POST",
            "/api/config/plugins",
            Some(json!({ "package": "ga2", "allow_unverified_publisher": true })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{second}");
        assert_eq!(second["version"]["plugin_key"], key.as_str());
        assert_eq!(second["version"]["active"], false);
        assert_eq!(second["version"]["approved"], true);

        let (status, listed) = send(&app, "GET", "/api/config/plugins", None).await;
        assert_eq!(status, StatusCode::OK);
        let plugins = listed["plugins"].as_array().unwrap();
        assert_eq!(plugins.len(), 1, "{listed}");
        assert_eq!(plugins[0]["plugin_key"], key.as_str());
        assert_eq!(plugins[0]["active_version"], "0.1.0");
        let rows = plugins[0]["versions"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.iter().filter(|r| r["active"] == true).count(),
            1,
            "{listed}"
        );

        // One plugin carries its pinned settings schema; an unknown key is 404.
        let plugin_uri = format!("/api/config/plugins/{}", segment(&key));
        let (status, one) = send(&app, "GET", &plugin_uri, None).await;
        assert_eq!(status, StatusCode::OK, "{one}");
        assert_eq!(one["id"], "graphicaudio");
        assert!(one["schema"]["settings"].is_array(), "{one}");
        let (status, _) = send(&app, "GET", "/api/config/plugins/path%3Anope", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // Moving the pointer updates; an unknown version is refused.
        let active_uri = format!("/api/config/plugins/{}/active", segment(&key));
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
            load_active_version(&store, &key)
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
            "/api/config/plugins/{}/versions/0.1.0/approve",
            segment(&key)
        );
        let (status, approved) = send(&app, "POST", &approve_uri, None).await;
        assert_eq!(status, StatusCode::OK, "{approved}");
        assert_eq!(approved["approved"], true);

        let (status, uninstalled) = send(&app, "DELETE", &plugin_uri, None).await;
        assert_eq!(status, StatusCode::OK, "{uninstalled}");
        assert_eq!(uninstalled["uninstalled"], true);
        assert!(load_active_version(&store, &key).await.unwrap().is_none());

        // Without the operator override the publisher is refused.
        let (status, refused) = send(
            &app,
            "POST",
            "/api/config/plugins",
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
            let (status, value) =
                send(&app, "POST", "/api/config/plugins", Some(body.clone())).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {value}");
        }
    }
}
