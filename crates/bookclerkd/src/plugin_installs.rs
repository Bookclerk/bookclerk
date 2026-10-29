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
use serde::{Deserialize, Serialize};

use crate::api::AppState;
use crate::config_authority::events_error as library_error;
use crate::http_error::ErrorMessage;

/// Operator actor recorded on install writes from the daemon.
fn operator_actor() -> ConfigActor {
    ConfigActor::Operator {
        id: "operator".into(),
    }
}

/// JSON error response.
fn error(status: StatusCode, message: impl std::fmt::Display) -> Response {
    (status, Json(ErrorMessage::new(message))).into_response()
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

/// One downloadable artifact of an installed version, as the API renders
/// it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct PluginArtifactSummary {
    /// Bookclerk target (`linux-x64-gnu`, …; `any` for workerd packages).
    #[schema(example = "linux-x64-gnu")]
    target: String,
    /// Where nodes fetch the archive: `https:`, `file:`, or
    /// `blob:sha256:<hex>`.
    #[schema(example = "https://plugins.example/source-libro-0.4.2-linux-x64-gnu.tar.gz")]
    url: String,
    /// Lowercase hex SHA-256 of the archive bytes.
    #[schema(example = "9b74c9897bac770ffc029102a200c5de3f4b2b1a0b5e6c7d8e9f0a1b2c3d4e5f")]
    archive_sha256: String,
    /// Payload-root digest of the extracted plugin tree.
    #[schema(example = "1f2e3d4c5b6a79880970a6b5c4d3e2f1a0b9c8d7e6f5a4b3c2d1e0f9a8b7c6d5")]
    payload_root_sha256: String,
}

/// One installed version of a plugin, as the API renders it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct PluginVersionSummary {
    /// Canonical plugin key.
    #[schema(example = "libro")]
    plugin_key: String,
    /// Manifest id (display alias).
    #[schema(example = "libro")]
    id: String,
    /// Semver package version.
    #[schema(example = "0.4.2")]
    version: String,
    /// True when this is the plugin's active version.
    #[schema(example = true)]
    active: bool,
    /// True once an operator approved the version's consent request.
    #[schema(example = true)]
    approved: bool,
    /// True when the publisher was verified at install time.
    #[schema(example = true)]
    publisher_verified: bool,
    /// Package coordinate the version was resolved from; empty for a local
    /// archive.
    #[schema(example = "registry:plugins.example#source-libro@0.4.2")]
    coordinate: String,
    /// RFC 3339 install time.
    #[schema(example = "2026-10-20T10:15:00Z")]
    installed_at: String,
    /// Audit identity of the installer.
    #[schema(example = "operator")]
    installed_by: String,
    /// Payload-root digest of the artifact this host would run; `null` when
    /// no artifact fits the host's target.
    #[schema(example = "1f2e3d4c5b6a79880970a6b5c4d3e2f1a0b9c8d7e6f5a4b3c2d1e0f9a8b7c6d5")]
    host_payload_root_sha256: Option<String>,
    /// Per-target artifacts.
    artifacts: Vec<PluginArtifactSummary>,
}

impl PluginVersionSummary {
    /// Projects `version`, flagged active or not.
    fn from_row(version: &PluginVersion, active: bool) -> Self {
        Self {
            plugin_key: version.plugin_key.clone(),
            id: version.manifest_id.clone(),
            version: version.version.clone(),
            active,
            approved: !version.grant_json.is_empty(),
            publisher_verified: version.publisher_verified,
            coordinate: version.coordinate.clone(),
            installed_at: version.installed_at.clone(),
            installed_by: version.installed_by.clone(),
            host_payload_root_sha256: host_artifact(version)
                .ok()
                .map(|a| a.payload_root_sha256.clone()),
            artifacts: version
                .artifacts
                .iter()
                .map(|a| PluginArtifactSummary {
                    target: a.target.clone(),
                    url: a.url.clone(),
                    archive_sha256: a.archive_sha256.clone(),
                    payload_root_sha256: a.payload_root_sha256.clone(),
                })
                .collect(),
        }
    }
}

/// One installed plugin: its active version and every installed version,
/// newest first.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct InstalledPluginSummary {
    /// Canonical plugin key.
    #[schema(example = "libro")]
    plugin_key: String,
    /// Manifest id (display alias) of the active version, or of the newest
    /// one when none is active; `null` without versions.
    #[schema(example = "libro")]
    id: Option<String>,
    /// Active version; `null` when the plugin is installed but not
    /// activated.
    #[schema(example = "0.4.2")]
    active_version: Option<String>,
    /// Compare-and-swap revision of the active-version pointer; `null`
    /// when nothing is active.
    #[schema(example = 3)]
    revision: Option<i64>,
    /// Every installed version, newest first.
    versions: Vec<PluginVersionSummary>,
}

impl InstalledPluginSummary {
    /// Projects `plugin_key` with its `versions` and active-version
    /// pointer.
    fn from_rows(
        plugin_key: &str,
        versions: &[&PluginVersion],
        install: Option<&PluginInstall>,
    ) -> Self {
        let active = install.map(|i| i.version.as_str());
        let alias = versions
            .iter()
            .find(|v| Some(v.version.as_str()) == active)
            .or_else(|| versions.first())
            .map(|v| v.manifest_id.clone());
        Self {
            plugin_key: plugin_key.to_string(),
            id: alias,
            active_version: active.map(str::to_string),
            revision: install.map(|i| i.revision),
            versions: versions
                .iter()
                .map(|v| PluginVersionSummary::from_row(v, Some(v.version.as_str()) == active))
                .collect(),
        }
    }
}

/// `GET /api/config/plugins` body.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct InstalledPluginsResponse {
    /// Every plugin with at least one installed version, by key.
    plugins: Vec<InstalledPluginSummary>,
}

/// The settings schema pinned for a plugin, as the API renders it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct PinnedSchemaSummary {
    /// Manifest id (display alias) at pin time.
    #[schema(example = "libro")]
    manifest_id: String,
    /// Fingerprint of the normalized schema.
    #[schema(example = "4e5f6a7b8c9d0e1f2a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091")]
    schema_sha256: String,
    /// Semver package version that produced the pinned schema.
    #[schema(example = "0.4.2")]
    package_version: String,
    /// RFC 3339 time of the last pin.
    #[schema(example = "2026-10-20T10:15:05Z")]
    recorded_at: String,
    /// Node that recorded the last pin.
    #[schema(example = "node-7f3a9c")]
    recorded_by_node: String,
    /// Declared settings.
    #[schema(value_type = Vec<crate::openapi::SettingSpecDoc>)]
    settings: Vec<bookclerk_plugin_abi::SettingSpec>,
}

/// `GET /api/config/plugins/{key}` body: the plugin and its pinned schema.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct InstalledPluginDetail {
    /// The plugin with its versions.
    #[serde(flatten)]
    plugin: InstalledPluginSummary,
    /// The pinned settings schema; `null` until a node pins one.
    schema: Option<PinnedSchemaSummary>,
}

/// `POST /api/config/plugins` body: what the install did.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct PluginInstalled {
    /// True when the version row was written now; false when the same
    /// version with the same artifacts was installed already.
    #[schema(example = true)]
    created: bool,
    /// Id of the plugin's default instance.
    #[schema(example = "3f9c2b7e-1d4a-4c8e-9b2f-6a7d8e9f0a1b")]
    plugin_instance_id: String,
    /// The installed version.
    version: PluginVersionSummary,
}

/// `PUT /api/config/plugins/{key}/active` body: the pointer after the move.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct ActiveVersionChanged {
    /// Canonical plugin key.
    #[schema(example = "libro")]
    plugin_key: String,
    /// Version now active.
    #[schema(example = "0.4.2")]
    version: String,
    /// Compare-and-swap revision of the pointer after the move.
    #[schema(example = 4)]
    revision: i64,
}

/// `DELETE /api/config/plugins/{key}` body.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct PluginUninstalled {
    /// Canonical plugin key.
    #[schema(example = "libro")]
    plugin_key: String,
    /// True when an active-version pointer existed and was cleared.
    #[schema(example = true)]
    uninstalled: bool,
    /// Deployment rows switched to `absent` across the plugin's instances.
    #[schema(example = 2)]
    deployments_undeployed: usize,
}

/// `POST /api/config/plugins/{key}/versions/{version}/approve` body.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct PluginVersionApproved {
    /// Canonical plugin key.
    #[schema(example = "libro")]
    plugin_key: String,
    /// Semver version whose grant was approved.
    #[schema(example = "0.4.2")]
    version: String,
    /// Always true: a version that cannot be approved is an error instead.
    #[schema(example = true)]
    approved: bool,
    /// The consent grant recorded for every node: the entrypoints, events,
    /// jobs, network domains and TCP targets the manifest asked for.
    #[schema(value_type = Object, example = json!({
        "schemaVersion": 1,
        "pluginKey": "libro",
        "pluginId": "libro",
        "entrypoints": ["source"],
        "producers": [],
        "consumers": [],
        "jobs": ["scan"],
        "networkMode": "domains",
        "domains": ["libro.fm", "api.libro.fm"],
        "manifestDomains": ["libro.fm", "api.libro.fm"],
        "operatorAddedDomains": [],
        "operatorDeniedDomains": [],
        "tcp": [],
        "manifestTcp": [],
        "operatorAddedTcp": []
    }))]
    grant: bookclerk_plugin_host::PluginGrant,
}

/// `GET /api/config/plugins`: every installed plugin.
#[utoipa::path(
    get,
    path = "/api/config/plugins",
    responses(
        (status = 200, description = "Every installed plugin.", body = InstalledPluginsResponse),
        (status = 500, description = "The configuration could not be read.", body = ErrorMessage),
    ),
)]
pub async fn get_plugins(
    State(state): State<Arc<AppState>>,
) -> Result<Json<InstalledPluginsResponse>, Response> {
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
            InstalledPluginSummary::from_rows(
                key,
                &of_key,
                installs.iter().find(|i| i.plugin_key == key),
            )
        })
        .collect();
    Ok(Json(InstalledPluginsResponse { plugins }))
}

/// `GET /api/config/plugins/{key}`: one installed plugin with its pinned
/// settings schema.
#[utoipa::path(
    get,
    path = "/api/config/plugins/{key}",
    params(("key" = String, Path, description = "Plugin key.")),
    responses(
        (status = 200, description = "The plugin with its installed versions and pinned schema.", body = InstalledPluginDetail),
        (status = 404, description = "No such plugin.", body = ErrorMessage),
        (status = 500, description = "The configuration could not be read.", body = ErrorMessage),
    ),
)]
pub async fn get_plugin(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Result<Json<InstalledPluginDetail>, Response> {
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
    let plugin = InstalledPluginSummary::from_rows(&key, &of_key, install.as_ref());
    let schema = load_plugin_schema(&library, &key)
        .await
        .map_err(library_error)?
        .map(|pinned| PinnedSchemaSummary {
            manifest_id: pinned.manifest_id,
            schema_sha256: pinned.schema_sha256,
            package_version: pinned.package_version,
            recorded_at: pinned.recorded_at,
            recorded_by_node: pinned.recorded_by_node,
            settings: pinned.schema.settings,
        });
    Ok(Json(InstalledPluginDetail { plugin, schema }))
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
        (status = 200, description = "The installed version.", body = PluginInstalled),
        (status = 400, description = "The body is invalid: neither or both of `coordinate` and `package`, a malformed coordinate or package name, or a manifest the host rejects.", body = ErrorMessage),
        (status = 403, description = "The publisher is unverified and `allow_unverified_publisher` is off.", body = ErrorMessage),
        (status = 404, description = "A local package is missing its `package.json` or `archive.tar.gz`.", body = ErrorMessage),
        (status = 409, description = "The version is already installed with different artifacts.", body = ErrorMessage),
        (status = 500, description = "The configuration could not be read or written.", body = ErrorMessage),
        (status = 502, description = "The package catalog could not be reached or answered badly.", body = ErrorMessage),
        (status = 503, description = "The install needs a helper that is unavailable.", body = ErrorMessage),
    ),
)]
pub async fn post_plugin_install(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PostPluginInstall>,
) -> Result<Json<PluginInstalled>, Response> {
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
    Ok(Json(PluginInstalled {
        created: installed.created,
        plugin_instance_id: installed.instance.id.to_string(),
        version: PluginVersionSummary::from_row(&installed.version, installed.active),
    }))
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
        (status = 200, description = "The active version after the change.", body = ActiveVersionChanged),
        (status = 400, description = "The body is invalid.", body = ErrorMessage),
        (status = 404, description = "No such plugin or version.", body = ErrorMessage),
        (status = 500, description = "The configuration could not be read or written.", body = ErrorMessage),
    ),
)]
pub async fn put_active_version(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Json(body): Json<PutActiveVersion>,
) -> Result<Json<ActiveVersionChanged>, Response> {
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
    Ok(Json(ActiveVersionChanged {
        plugin_key: pointer.plugin_key,
        version: pointer.version,
        revision: pointer.revision,
    }))
}

/// `DELETE /api/config/plugins/{key}`: uninstall the plugin (active version
/// cleared, placements dropped, every deployment `absent`).
#[utoipa::path(
    delete,
    path = "/api/config/plugins/{key}",
    params(("key" = String, Path, description = "Plugin key.")),
    responses(
        (status = 200, description = "The plugin was uninstalled.", body = PluginUninstalled),
        (status = 404, description = "No such plugin.", body = ErrorMessage),
        (status = 409, description = "A deployment changed under the request. The install was not cleared, but deployments of earlier instances may already be stopped; retry the request.", body = ErrorMessage),
        (status = 500, description = "The configuration could not be read or written.", body = ErrorMessage),
    ),
)]
pub async fn delete_plugin_install(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Result<Json<PluginUninstalled>, Response> {
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
    Ok(Json(PluginUninstalled {
        plugin_key: key,
        uninstalled: cleared,
        deployments_undeployed: disabled,
    }))
}

/// `POST /api/config/plugins/{key}/versions/{version}/approve`: record the
/// version's requested consent as approved for every node.
#[utoipa::path(
    post,
    path = "/api/config/plugins/{key}/versions/{version}/approve",
    params(("key" = String, Path, description = "Plugin key."), ("version" = String, Path, description = "Semver version.")),
    responses(
        (status = 200, description = "The version's grant was approved.", body = PluginVersionApproved),
        (status = 400, description = "The version's manifest does not parse.", body = ErrorMessage),
        (status = 404, description = "No such plugin or version.", body = ErrorMessage),
        (status = 500, description = "The configuration could not be read or written.", body = ErrorMessage),
    ),
)]
pub async fn approve_plugin_version(
    State(state): State<Arc<AppState>>,
    Path((key, version)): Path<(String, String)>,
) -> Result<Json<PluginVersionApproved>, Response> {
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
    Ok(Json(PluginVersionApproved {
        plugin_key: row.plugin_key,
        version: row.version,
        approved: true,
        grant,
    }))
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
