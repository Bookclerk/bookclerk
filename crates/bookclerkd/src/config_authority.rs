//! Runtime application of database-backed `[events]` configuration.
//!
//! The dispatcher and pruner read [`bookclerk_config::Config::events`] on each
//! tick. This module publishes committed revisions into that struct. Local
//! delivery-task count is chosen when the event runtime starts and stays at
//! that count until process restart; retention and the in-flight cap follow
//! the latest applied revision.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use bookclerk_library::control_plane::{
    load_cluster_row, load_events, overlay_events, replace_events, ConfigActor, EventsReplace,
    EventsSettingsV1,
};
use bookclerk_library::LibraryStore;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::api::AppState;

/// Operator actor recorded on configuration commits from the daemon.
fn operator_actor() -> ConfigActor {
    ConfigActor::Operator {
        id: "operator".into(),
    }
}

/// Re-reads `core.events` on each `configuration.changed` delivery on this
/// node's queue, plus a slow backstop pass, until the process stops.
pub fn spawn_config_reconciler(state: Arc<AppState>) {
    crate::node_queue::spawn_woken_loop(
        state,
        crate::node_queue::config_wait,
        |state| async move {
            if let Err(err) = reconcile_events(&state).await {
                tracing::warn!(error = %err, "configuration reconcile failed");
            }
        },
    );
}

/// How a loaded document may replace in-memory `[events]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EventsPublish {
    /// Apply only a strictly newer revision of the same cluster.
    Monotonic,
    /// Install this database's document, even when its revision is lower.
    DatabaseSwap,
}

/// One `core.events` read tied to the cluster that stored it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LoadedEvents {
    /// `cluster_identity.cluster_id` read with the document.
    pub cluster_id: String,
    /// Typed document and revision.
    pub document: bookclerk_library::control_plane::ConfigurationDocument<EventsSettingsV1>,
}

// While set on this task, publication reads fail before touching the database.
#[cfg(test)]
tokio::task_local! {
    pub(crate) static FAIL_EVENTS_PUBLICATION_READ: ();
}

/// Reads `core.events` and the cluster id that owns it.
///
/// Callers keep this value and publish it later. A newer publication can land
/// in between; [`publish_loaded`] then refuses to move the effective revision
/// backwards.
///
/// # Errors
///
/// Returns an error when the cluster row or the document cannot be read. The
/// in-memory config is left unchanged.
pub(crate) async fn load_events_publication(
    library: &LibraryStore,
) -> anyhow::Result<LoadedEvents> {
    #[cfg(test)]
    if FAIL_EVENTS_PUBLICATION_READ.try_with(|_| ()).is_ok() {
        anyhow::bail!("injected events publication read failure");
    }
    let row = load_cluster_row(library)
        .await?
        .ok_or_else(|| anyhow::anyhow!("cluster identity is not initialized"))?;
    let document = load_events(library).await?;
    Ok(LoadedEvents {
        cluster_id: row.cluster_id,
        document,
    })
}

/// Applies `loaded` onto `config` when `mode` allows it.
///
/// Returns whether the effective body changed. Equal revisions of the same
/// cluster are left alone. A different cluster is applied only for
/// [`EventsPublish::DatabaseSwap`].
pub(crate) fn install_events(
    config: &mut bookclerk_config::Config,
    loaded: &LoadedEvents,
    mode: EventsPublish,
) -> bool {
    match mode {
        EventsPublish::DatabaseSwap => {
            overlay_events(config, &loaded.document, &loaded.cluster_id);
            true
        }
        EventsPublish::Monotonic => {
            if let Some(current) = config.events_authority.as_deref() {
                if current != loaded.cluster_id {
                    return false;
                }
                if config
                    .events_revision
                    .is_some_and(|revision| loaded.document.revision <= revision)
                {
                    return false;
                }
            }
            overlay_events(config, &loaded.document, &loaded.cluster_id);
            true
        }
    }
}

/// Merges a freshly read document into `candidate` without regressing `live`.
///
/// `database_swap` is the explicit reconnect signal. A cluster id that differs
/// from `live` is also a swap, so a stale revision number from the previous
/// database cannot win by being larger.
pub(crate) fn finish_reload_events(
    live: &bookclerk_config::Config,
    candidate: &mut bookclerk_config::Config,
    fresh: &LoadedEvents,
    database_swap: bool,
) {
    candidate.events = live.events.clone();
    candidate.events_revision = live.events_revision;
    candidate.events_authority = live.events_authority.clone();
    let mode = if database_swap
        || live
            .events_authority
            .as_deref()
            .is_some_and(|id| id != fresh.cluster_id)
    {
        EventsPublish::DatabaseSwap
    } else {
        EventsPublish::Monotonic
    };
    install_events(candidate, fresh, mode);
}

/// Publishes `loaded` under the config write lock.
///
/// The lock is the barrier between this publication and reload's final assign.
pub(crate) async fn publish_loaded(
    state: &AppState,
    loaded: &LoadedEvents,
    mode: EventsPublish,
) -> bool {
    let mut config = state.config.write().await;
    let applied = install_events(&mut config, loaded, mode);
    if applied {
        tracing::info!(
            revision = loaded.document.revision,
            cluster_id = %loaded.cluster_id,
            "applied core.events from the database"
        );
    }
    applied
}

/// Applies a newer cluster events document onto the live config.
///
/// The read and the publication are separate so a newer revision can commit
/// before a stale read is released. The stale publication then leaves the
/// effective body and revision unchanged.
///
/// # Errors
///
/// Returns an error when the document cannot be read. The in-memory config is
/// left unchanged, so a bad revision is not announced as applied.
pub async fn reconcile_events(state: &AppState) -> anyhow::Result<()> {
    let library = state.library_snapshot().await;
    let loaded = load_events_publication(&library).await?;
    publish_loaded(state, &loaded, EventsPublish::Monotonic).await;
    Ok(())
}

/// JSON body for `PUT /api/config/domains/core.events`.
#[derive(Debug, Deserialize)]
pub struct PutEventsDomainRequest {
    /// Revision the caller read. A mismatch is a conflict.
    pub expected_revision: i64,
    /// Replacement `[events]` document.
    pub retention_days: u64,
    /// Replacement dead-letter retention.
    pub dead_letter_retention_days: u64,
    /// Replacement concurrency cap.
    pub concurrency: u32,
}

/// `GET /api/config/domains/core.events`.
pub async fn get_events_domain(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, Response> {
    let library = state.library_snapshot().await;
    let doc = load_events(&library).await.map_err(events_error)?;
    Ok(Json(events_json(&doc)))
}

/// `PUT /api/config/domains/core.events`.
pub async fn put_events_domain(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PutEventsDomainRequest>,
) -> Result<Json<serde_json::Value>, Response> {
    let replacement = EventsSettingsV1 {
        retention_days: body.retention_days,
        dead_letter_retention_days: body.dead_letter_retention_days,
        concurrency: body.concurrency,
    };
    let library = state.library_snapshot().await;
    let outcome = replace_events(
        &library,
        &operator_actor(),
        body.expected_revision,
        &replacement,
        &uuid::Uuid::new_v4().to_string(),
    )
    .await
    .map_err(events_error)?;
    match outcome {
        EventsReplace::Applied(doc) => {
            let cluster_id = cluster_id_of(&library).await?;
            publish_events(state.as_ref(), &doc, &cluster_id).await;
            Ok(Json(events_json(&doc)))
        }
        EventsReplace::Replayed { revision } => Ok(Json(json!({
            "namespace": "core.events",
            "replayed": true,
            "revision": revision,
        }))),
        EventsReplace::Conflict { current_revision } => Err(conflict(current_revision)),
    }
}

/// Applies `events.*` settings keys through compare-and-swap.
///
/// # Errors
///
/// Returns 400 for invalid values and 409 when `expected` revision lost.
pub async fn commit_events_settings(
    state: &AppState,
    updates: &[(String, String)],
) -> Result<(), Response> {
    if updates.is_empty() {
        return Ok(());
    }
    let library = state.library_snapshot().await;
    let current = load_events(&library).await.map_err(events_error)?;
    let mut body = current.body.clone();
    for (key, value) in updates {
        apply_events_key(&mut body, key, value).map_err(|err| {
            tracing::warn!(error = %err, "rejected events settings update");
            StatusCode::BAD_REQUEST.into_response()
        })?;
    }
    if body == current.body {
        return Ok(());
    }
    let outcome = replace_events(
        &library,
        &operator_actor(),
        current.revision,
        &body,
        &uuid::Uuid::new_v4().to_string(),
    )
    .await
    .map_err(events_error)?;
    match outcome {
        EventsReplace::Applied(doc) => {
            let cluster_id = cluster_id_of(&library).await?;
            publish_events(state, &doc, &cluster_id).await;
            Ok(())
        }
        EventsReplace::Replayed { .. } => Ok(()),
        EventsReplace::Conflict { current_revision } => Err(conflict(current_revision)),
    }
}

/// Copies a committed events document into the live daemon config.
async fn publish_events(
    state: &AppState,
    doc: &bookclerk_library::control_plane::ConfigurationDocument<EventsSettingsV1>,
    cluster_id: &str,
) {
    let loaded = LoadedEvents {
        cluster_id: cluster_id.to_string(),
        document: doc.clone(),
    };
    publish_loaded(state, &loaded, EventsPublish::Monotonic).await;
}

/// Cluster id stored beside the secret fingerprint.
async fn cluster_id_of(library: &LibraryStore) -> Result<String, Response> {
    let row = load_cluster_row(library).await.map_err(events_error)?;
    row.map(|row| row.cluster_id).ok_or_else(|| {
        events_error(bookclerk_library::LibraryError::Other(anyhow::anyhow!(
            "cluster identity is not initialized"
        )))
    })
}

/// Applies one `events.*` settings key onto a typed document.
fn apply_events_key(body: &mut EventsSettingsV1, key: &str, value: &str) -> Result<(), String> {
    match key {
        "events.retention_days" => {
            body.retention_days = value
                .parse()
                .map_err(|_| "events.retention_days must be an integer".to_string())?;
        }
        "events.dead_letter_retention_days" => {
            body.dead_letter_retention_days = value
                .parse()
                .map_err(|_| "events.dead_letter_retention_days must be an integer".to_string())?;
        }
        "events.concurrency" => {
            body.concurrency = value
                .parse()
                .map_err(|_| "events.concurrency must be an integer".to_string())?;
        }
        other => return Err(format!("unsupported events setting key: {other}")),
    }
    body.validate().map_err(|err| err.to_string())
}

/// JSON body for the events domain API.
fn events_json(
    doc: &bookclerk_library::control_plane::ConfigurationDocument<EventsSettingsV1>,
) -> serde_json::Value {
    json!({
        "namespace": doc.namespace,
        "scope_type": doc.scope_type,
        "scope_id": doc.scope_id,
        "schema_version": doc.schema_version,
        "revision": doc.revision,
        "updated_at": doc.updated_at,
        "updated_by": doc.updated_by,
        "retention_days": doc.body.retention_days,
        "dead_letter_retention_days": doc.body.dead_letter_retention_days,
        "concurrency": doc.body.concurrency,
    })
}

/// Re-reads local plugin deployments on each `deployment.changed` or
/// `configuration.changed` delivery on this node's queue, plus a slow
/// backstop pass, until the process stops.
pub fn spawn_deployment_reconciler(state: Arc<AppState>) {
    crate::node_queue::spawn_woken_loop(
        state,
        crate::node_queue::deployment_wait,
        |state| async move {
            if let Err(err) = reconcile_deployments(&state).await {
                tracing::warn!(error = %err, "plugin deployment reconcile failed");
            }
        },
    );
}

/// Reconciles this node's deployments: installs, spawns, respawns, and
/// retires guests until the live registries match `plugin_deployments`.
///
/// One pass runs at a time; the periodic tick and an on-demand pass after a
/// config reload queue behind each other.
pub(crate) async fn reconcile_deployments(state: &AppState) -> anyhow::Result<()> {
    let runtime = state.deployment_runtime();
    let _pass = runtime.reconcile_lock().lock().await;
    let config = state.config.read().await.clone();
    let store = state.library.read().await.clone();
    let node =
        bookclerk_library::control_plane::load_or_create_node_identity(&config.paths().files_dir)?;
    let packages = bookclerk_plugin_host::load_authorized_local_packages(&config.paths().files_dir)
        .map_err(|err| anyhow::anyhow!(err))?;
    bookclerk_plugin_host::reconcile_local_deployments(
        &store,
        &config,
        &node.node_id,
        &packages,
        runtime.as_ref(),
    )
    .await?;
    Ok(())
}

/// This node's view of one deployment row, with its latest observation.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct PluginDeploymentView {
    /// Deployment row id.
    pub deployment_id: String,
    /// Node the row targets.
    pub node_id: String,
    /// `present` or `absent`.
    pub desired: String,
    /// Compare-and-swap revision of the desired row.
    pub revision: i64,
    /// Latest observation status from that node, when it reported one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Observation detail (error text, redacted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Document revision the running guest applied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied_config_revision: Option<i64>,
    /// RFC 3339 time of the observation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
}

/// Instance document as the API renders it: values and secret *names* only.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct InstanceConfigView {
    /// Compare-and-swap revision.
    pub revision: i64,
    /// Scalar settings the operator wrote.
    pub settings:
        std::collections::BTreeMap<String, bookclerk_library::control_plane::SettingValue>,
    /// Secret refs; ciphertext stays in `encrypted_secrets`.
    pub secret_refs: Vec<bookclerk_library::control_plane::InstanceSecretRefV1>,
}

/// Scheduler placement of one instance: the operator's intent and the last
/// pass's result.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct PlacementView {
    /// Nodes that should run the instance.
    pub replicas: bookclerk_library::control_plane::Replicas,
    /// Label constraints every chosen node must satisfy.
    pub constraints: Vec<String>,
    /// Compare-and-swap revision of the intent.
    pub revision: i64,
    /// Replicas the last scheduler pass placed.
    pub placed: u32,
    /// Why fewer than requested are placed; empty when satisfied.
    pub detail: String,
    /// RFC 3339 time the scheduler last changed the status.
    pub scheduled_at: String,
    /// RFC 3339 time of the last intent write.
    pub updated_at: String,
}

impl From<bookclerk_library::control_plane::PluginPlacement> for PlacementView {
    fn from(p: bookclerk_library::control_plane::PluginPlacement) -> Self {
        Self {
            replicas: p.spec.replicas,
            constraints: p.spec.constraints,
            revision: p.revision,
            placed: p.placed,
            detail: p.detail,
            scheduled_at: p.scheduled_at,
            updated_at: p.updated_at,
        }
    }
}

/// One plugin instance with its schema, document, and deployments.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct PluginInstanceView {
    /// Instance id.
    pub plugin_instance_id: String,
    /// Canonical plugin key.
    pub plugin_key: String,
    /// Manifest id recorded with the pinned schema (display alias).
    pub alias: String,
    /// Operator-facing name; empty for the default instance.
    pub display_name: String,
    /// True when a bare plugin key or alias routes to this instance.
    pub is_default: bool,
    /// RFC 3339 creation time.
    pub created_at: String,
    /// Declared settings from the pinned schema.
    pub schema: Vec<bookclerk_plugin_abi::SettingSpec>,
    /// Current document, when the instance has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<InstanceConfigView>,
    /// Deployment rows across nodes.
    pub deployments: Vec<PluginDeploymentView>,
    /// Scheduler placement; absent when the instance is hand-managed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placement: Option<PlacementView>,
}

/// Every plugin instance joined with its pinned schema, document, and
/// deployments. `node_id` selects whose observation each deployment carries.
///
/// # Errors
///
/// Returns an error when a control-plane read fails.
pub(crate) async fn list_plugin_instance_views(
    library: &LibraryStore,
    node_id: &str,
) -> bookclerk_library::Result<Vec<PluginInstanceView>> {
    use bookclerk_library::control_plane::{
        default_instance_for_key, list_all_plugin_instances, list_deployments_for_instance,
        list_plugin_placements, list_plugin_schemas, load_instance_config, load_observation,
    };
    let mut placements: std::collections::HashMap<String, PlacementView> =
        list_plugin_placements(library)
            .await?
            .into_iter()
            .map(|p| (p.plugin_instance_id.to_string(), PlacementView::from(p)))
            .collect();
    let schemas: std::collections::HashMap<
        String,
        bookclerk_library::control_plane::PinnedPluginSchema,
    > = list_plugin_schemas(library)
        .await?
        .into_iter()
        .map(|schema| (schema.plugin_key.clone(), schema))
        .collect();
    let mut defaults: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut views = Vec::new();
    for instance in list_all_plugin_instances(library).await? {
        if !defaults.contains_key(&instance.plugin_key) {
            if let Some(default) = default_instance_for_key(library, &instance.plugin_key).await? {
                defaults.insert(instance.plugin_key.clone(), default.id.to_string());
            }
        }
        let pinned = schemas.get(&instance.plugin_key);
        let config = match load_instance_config(library, &instance.id).await {
            Ok(document) => Some(InstanceConfigView {
                revision: document.revision,
                settings: document.body.settings,
                secret_refs: document.body.secret_refs,
            }),
            Err(bookclerk_library::LibraryError::NotFound(_)) => None,
            Err(err) => return Err(err),
        };
        let mut deployments = Vec::new();
        for deployment in list_deployments_for_instance(library, &instance.id).await? {
            let observation = if deployment.node_id == node_id {
                load_observation(library, &deployment.deployment_id, node_id).await?
            } else {
                load_observation(library, &deployment.deployment_id, &deployment.node_id).await?
            };
            deployments.push(PluginDeploymentView {
                deployment_id: deployment.deployment_id,
                node_id: deployment.node_id,
                desired: deployment.desired.as_str().to_string(),
                revision: deployment.revision,
                status: observation.as_ref().map(|o| o.status.as_str().to_string()),
                detail: observation
                    .as_ref()
                    .map(|o| o.detail.clone())
                    .filter(|d| !d.is_empty()),
                applied_config_revision: observation
                    .as_ref()
                    .and_then(|o| o.applied_config_revision),
                observed_at: observation.as_ref().map(|o| o.observed_at.clone()),
            });
        }
        views.push(PluginInstanceView {
            plugin_instance_id: instance.id.to_string(),
            plugin_key: instance.plugin_key.clone(),
            alias: pinned
                .map(|schema| schema.manifest_id.clone())
                .unwrap_or_default(),
            display_name: instance.display_name.clone(),
            is_default: defaults
                .get(&instance.plugin_key)
                .is_some_and(|id| *id == instance.id.as_str()),
            created_at: instance.created_at.clone(),
            schema: pinned
                .map(|schema| schema.schema.settings.clone())
                .unwrap_or_default(),
            config,
            deployments,
            placement: placements.remove(instance.id.as_str()),
        });
    }
    Ok(views)
}

/// This node's id from `node-identity.json`.
fn this_node_id(config: &bookclerk_config::Config) -> Result<String, Response> {
    bookclerk_library::control_plane::load_or_create_node_identity(&config.paths().files_dir)
        .map(|node| node.node_id)
        .map_err(events_error)
}

/// `GET /api/config/plugin-instances`.
pub async fn get_plugin_instances(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, Response> {
    let cfg = state.config.read().await.clone();
    let node_id = this_node_id(&cfg)?;
    let library = state.library_snapshot().await;
    let instances = list_plugin_instance_views(&library, &node_id)
        .await
        .map_err(events_error)?;
    Ok(Json(json!({
        "node_id": node_id,
        "instances": instances,
    })))
}

/// `POST /api/config/plugin-instances` body.
#[derive(Debug, Deserialize)]
pub(crate) struct CreatePluginInstance {
    /// Canonical plugin key; the plugin must have a pinned schema.
    plugin_key: String,
    /// Operator-facing name.
    #[serde(default)]
    display_name: String,
}

/// `POST /api/config/plugin-instances`: a second (non-default) instance of
/// a plugin, with an empty document so schema defaults apply.
pub async fn post_plugin_instance(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreatePluginInstance>,
) -> Result<Json<serde_json::Value>, Response> {
    use bookclerk_library::control_plane::{
        create_plugin_instance, import_instance_config_if_absent, require_plugin_schema,
        PluginInstanceConfigV1,
    };
    let library = state.library_snapshot().await;
    let pinned = require_plugin_schema(&library, body.plugin_key.trim())
        .await
        .map_err(events_error)?;
    let instance = create_plugin_instance(
        &library,
        &operator_actor(),
        &pinned.plugin_key,
        body.display_name.trim(),
    )
    .await
    .map_err(events_error)?;
    import_instance_config_if_absent(
        &library,
        &operator_actor(),
        &instance.id,
        &pinned.schema,
        &PluginInstanceConfigV1::empty(),
        &format!("seed-instance-config-{}", instance.id),
    )
    .await
    .map_err(events_error)?;
    Ok(Json(json!({
        "plugin_instance_id": instance.id.to_string(),
        "plugin_key": instance.plugin_key,
        "display_name": instance.display_name,
    })))
}

/// `DELETE /api/config/plugin-instances/{id}`: removes an instance that has
/// no deployment left.
pub async fn delete_plugin_instance_route(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, Response> {
    use bookclerk_library::control_plane::{delete_plugin_instance, PluginInstanceId};
    let instance_id = PluginInstanceId::parse(&id).map_err(events_error)?;
    let library = state.library_snapshot().await;
    delete_plugin_instance(&library, &operator_actor(), &instance_id)
        .await
        .map_err(events_error)?;
    Ok(Json(
        json!({ "plugin_instance_id": instance_id.to_string(), "deleted": true }),
    ))
}

/// `GET /api/config/plugin-instances/{id}/config`.
pub async fn get_plugin_instance_config(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, Response> {
    use bookclerk_library::control_plane::{
        load_instance_config, load_plugin_instance, load_plugin_schema, PluginInstanceId,
    };
    let instance_id = PluginInstanceId::parse(&id).map_err(events_error)?;
    let library = state.library_snapshot().await;
    let instance = load_plugin_instance(&library, &instance_id)
        .await
        .map_err(events_error)?
        .ok_or_else(|| {
            events_error(bookclerk_library::LibraryError::NotFound(format!(
                "plugin instance {instance_id}"
            )))
        })?;
    let document = load_instance_config(&library, &instance_id)
        .await
        .map_err(events_error)?;
    let schema = load_plugin_schema(&library, &instance.plugin_key)
        .await
        .map_err(events_error)?;
    let effective = schema
        .as_ref()
        .map(|pinned| {
            let settings: std::collections::BTreeMap<String, serde_json::Value> = document
                .body
                .settings
                .iter()
                .map(|(key, value)| (key.clone(), setting_value_json(value)))
                .collect();
            pinned.schema.effective_config(&settings).config
        })
        .unwrap_or_default();
    Ok(Json(json!({
        "plugin_instance_id": instance_id.to_string(),
        "plugin_key": instance.plugin_key,
        "revision": document.revision,
        "settings": document.body.settings,
        "secret_refs": document.body.secret_refs,
        "effective": effective,
        "schema": schema.map(|pinned| pinned.schema.settings).unwrap_or_default(),
    })))
}

/// JSON form of a stored setting value.
fn setting_value_json(value: &bookclerk_library::control_plane::SettingValue) -> serde_json::Value {
    use bookclerk_library::control_plane::SettingValue;
    match value {
        SettingValue::Bool(flag) => serde_json::Value::Bool(*flag),
        SettingValue::Number(number) => serde_json::Value::Number(number.clone()),
        SettingValue::String(text) => serde_json::Value::String(text.clone()),
    }
}

/// `PUT /api/config/plugin-instances/{id}/config` body.
#[derive(Debug, Deserialize)]
pub(crate) struct PutPluginInstanceConfig {
    /// Revision the caller last observed.
    expected_revision: i64,
    /// Scalar settings. Secret values are not accepted here.
    #[serde(default)]
    settings: std::collections::BTreeMap<String, bookclerk_library::control_plane::SettingValue>,
    /// Secret refs to keep. Omitted refs are dropped from the document; the
    /// ciphertext stays sealed until the secret is overwritten.
    #[serde(default)]
    secret_refs: Vec<bookclerk_library::control_plane::InstanceSecretRefV1>,
    /// Secret values to seal, keyed by the declared `secret` setting. Each
    /// is sealed into `encrypted_secrets` and referenced from the document;
    /// the plaintext is never stored in the document or echoed back.
    #[serde(default)]
    secrets: std::collections::BTreeMap<String, String>,
}

/// `PUT /api/config/plugin-instances/{id}/config`.
///
/// Operator principal only. The route is mounted on the operator router.
/// The document is validated against the plugin's pinned schema, so this
/// node does not need the guest installed.
pub async fn put_plugin_instance_config(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<PutPluginInstanceConfig>,
) -> Result<Json<serde_json::Value>, Response> {
    use bookclerk_library::control_plane::{
        load_instance_config, load_plugin_instance, replace_instance_config, require_plugin_schema,
        seal_instance_secret, set_secret_ref, InstanceConfigReplace, PluginInstanceConfigV1,
        PluginInstanceId,
    };
    let instance_id = PluginInstanceId::parse(&id).map_err(events_error)?;
    let library = state.library_snapshot().await;
    let instance = load_plugin_instance(&library, &instance_id)
        .await
        .map_err(events_error)?
        .ok_or_else(|| {
            events_error(bookclerk_library::LibraryError::NotFound(format!(
                "plugin instance {instance_id}"
            )))
        })?;
    let pinned = require_plugin_schema(&library, &instance.plugin_key)
        .await
        .map_err(events_error)?;
    let mut document = PluginInstanceConfigV1 {
        settings: body.settings,
        secret_refs: body.secret_refs,
    };
    // Validate and commit the document first; a rejected write (stale
    // revision, invalid value) must leave the live secrets untouched. The
    // ciphertext is sealed only after the compare-and-swap succeeds.
    for (key, plaintext) in &body.secrets {
        pinned.schema.check_secret_key(key).map_err(|err| {
            events_error(bookclerk_library::LibraryError::Other(anyhow::anyhow!(
                "invalid configuration: {err}"
            )))
        })?;
        if plaintext.is_empty() {
            return Err(events_error(bookclerk_library::LibraryError::Other(
                anyhow::anyhow!("invalid configuration: secret `{key}` is empty"),
            )));
        }
        set_secret_ref(&mut document, key);
    }
    document.validate(&pinned.schema).map_err(events_error)?;
    let outcome = replace_instance_config(
        &library,
        &operator_actor(),
        &instance_id,
        &pinned.schema,
        body.expected_revision,
        &document,
        &uuid::Uuid::new_v4().to_string(),
    )
    .await
    .map_err(events_error)?;
    let revision = match outcome {
        InstanceConfigReplace::Applied(doc) => doc.revision,
        InstanceConfigReplace::Replayed { revision } => revision,
        InstanceConfigReplace::Conflict { current_revision } => {
            return Err(conflict(current_revision))
        }
    };
    for (key, plaintext) in &body.secrets {
        seal_instance_secret(&library, &instance_id, &instance.plugin_key, key, plaintext)
            .await
            .map_err(|err| {
                events_error(bookclerk_library::LibraryError::Other(anyhow::anyhow!(
                    "document revision {revision} was saved but secret `{key}` was not stored \
                     ({err}); submit the secret again"
                )))
            })?;
    }
    let doc = load_instance_config(&library, &instance_id)
        .await
        .map_err(events_error)?;
    Ok(Json(json!({
        "plugin_instance_id": instance_id.to_string(),
        "revision": doc.revision,
        "settings": doc.body.settings,
        "secret_refs": doc.body.secret_refs,
    })))
}

/// `PUT /api/config/plugin-instances/{id}/deployments/{node_id}` body.
#[derive(Debug, Deserialize)]
pub(crate) struct PutPluginDeployment {
    /// `present` or `absent`.
    desired: String,
    /// Revision the caller last observed; required when the row exists and
    /// the caller wants compare-and-swap. Omitted means "replace whatever is
    /// current".
    #[serde(default)]
    expected_revision: Option<i64>,
}

/// `PUT /api/config/plugin-instances/{id}/deployments/{node_id}`: the
/// enable/disable switch. Creates the row when the node has none.
pub async fn put_plugin_deployment(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((id, node_id)): axum::extract::Path<(String, String)>,
    Json(body): Json<PutPluginDeployment>,
) -> Result<Json<serde_json::Value>, Response> {
    use bookclerk_library::control_plane::{
        ensure_plugin_deployment, list_node_ids, load_deployment_for_node, load_plugin_instance,
        replace_plugin_deployment, DeploymentReplace, PluginInstanceId,
    };
    let instance_id = PluginInstanceId::parse(&id).map_err(events_error)?;
    let library = state.library_snapshot().await;
    load_plugin_instance(&library, &instance_id)
        .await
        .map_err(events_error)?
        .ok_or_else(|| {
            events_error(bookclerk_library::LibraryError::NotFound(format!(
                "plugin instance {instance_id}"
            )))
        })?;
    bookclerk_library::control_plane::ensure_hand_managed(&library, &instance_id)
        .await
        .map_err(events_error)?;
    let node_id = node_id.trim().to_string();
    if !list_node_ids(&library)
        .await
        .map_err(events_error)?
        .contains(&node_id)
    {
        return Err(events_error(bookclerk_library::LibraryError::NotFound(
            format!("node {node_id}"),
        )));
    }
    let desired = bookclerk_library::control_plane::DesiredState::parse(&body.desired)
        .map_err(events_error)?;
    let existing = load_deployment_for_node(&library, &instance_id, &node_id)
        .await
        .map_err(events_error)?;
    let row = match existing {
        None => {
            ensure_plugin_deployment(&library, &operator_actor(), &instance_id, &node_id, desired)
                .await
                .map_err(events_error)?
        }
        Some(current) if current.desired == desired && body.expected_revision.is_none() => current,
        Some(current) => {
            let expected = body.expected_revision.unwrap_or(current.revision);
            match replace_plugin_deployment(
                &library,
                &operator_actor(),
                &current.deployment_id,
                expected,
                desired,
                &uuid::Uuid::new_v4().to_string(),
            )
            .await
            .map_err(events_error)?
            {
                DeploymentReplace::Applied(row) => row,
                DeploymentReplace::Replayed { .. } => current,
                DeploymentReplace::Conflict { current_revision } => {
                    return Err(conflict(current_revision))
                }
            }
        }
    };
    Ok(Json(json!({
        "deployment_id": row.deployment_id,
        "plugin_instance_id": row.plugin_instance_id.to_string(),
        "node_id": row.node_id,
        "desired": row.desired.as_str(),
        "revision": row.revision,
    })))
}

/// `GET /api/config/nodes`: every node that registered with the cluster.
pub async fn get_nodes(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, Response> {
    let cfg = state.config.read().await.clone();
    let this_node = this_node_id(&cfg)?;
    let library = state.library_snapshot().await;
    let statuses = node_statuses(&library).await?;
    let mut nodes = Vec::new();
    for node in bookclerk_library::control_plane::list_nodes(&library)
        .await
        .map_err(events_error)?
    {
        let runtime = node_runtime_or_default(&library, &node.node_id).await?;
        nodes.push(node_json(
            &node,
            &this_node,
            statuses.get(&node.node_id),
            &runtime,
        ));
    }
    Ok(Json(json!({ "node_id": this_node, "nodes": nodes })))
}

/// Every node's latest heartbeat, by node id.
async fn node_statuses(
    library: &LibraryStore,
) -> Result<std::collections::HashMap<String, bookclerk_library::control_plane::NodeStatus>, Response>
{
    Ok(
        bookclerk_library::control_plane::list_node_statuses(library)
            .await
            .map_err(events_error)?
            .into_iter()
            .map(|status| (status.node_id.clone(), status))
            .collect(),
    )
}

/// A node's runtime document, or the defaults when it never wrote one.
async fn node_runtime_or_default(
    library: &LibraryStore,
    node_id: &str,
) -> Result<bookclerk_library::control_plane::NodeRuntimeSettingsV1, Response> {
    match bookclerk_library::control_plane::load_node_runtime(library, node_id).await {
        Ok(document) => Ok(document.body),
        Err(bookclerk_library::LibraryError::NotFound(_)) => {
            Ok(bookclerk_library::control_plane::NodeRuntimeSettingsV1::new())
        }
        Err(err) => Err(events_error(err)),
    }
}

/// JSON projection of one node row with its latest heartbeat and budget.
fn node_json(
    node: &bookclerk_library::control_plane::NodeRecord,
    this_node: &str,
    status: Option<&bookclerk_library::control_plane::NodeStatus>,
    runtime: &bookclerk_library::control_plane::NodeRuntimeSettingsV1,
) -> serde_json::Value {
    let now_ms = chrono::Utc::now().timestamp_millis();
    json!({
        "node_id": node.node_id,
        "cluster_id": node.cluster_id,
        "created_at": node.created_at,
        "heartbeat_at": node.heartbeat_at,
        "software_version": node.software_version,
        "schema_state": node.schema_state,
        "compatible": node.compatible,
        "target": node.target,
        "labels": node.labels,
        "implicit_labels": bookclerk_library::control_plane::effective_node_labels(node)
            .into_iter()
            .filter(|label| !node.labels.contains(label))
            .collect::<Vec<_>>(),
        "live": status.is_some_and(|s| s.is_live(now_ms)),
        "runtime": runtime,
        "status": status,
        "this_node": node.node_id == this_node,
    })
}

/// `PUT /api/config/nodes/{node_id}/runtime` body: any subset of the
/// node's runtime document.
#[derive(Debug, Deserialize)]
pub(crate) struct PutNodeRuntime {
    /// Operator label.
    #[serde(default)]
    label: Option<String>,
    /// Share of detected CPU the scheduler may commit (1..=100).
    #[serde(default)]
    cpu_budget_percent: Option<u8>,
    /// Share of detected memory the scheduler may commit (1..=100).
    #[serde(default)]
    memory_budget_percent: Option<u8>,
    /// False drains the node.
    #[serde(default)]
    schedulable: Option<bool>,
}

/// `PUT /api/config/nodes/{node_id}/runtime`: edit a node's label, budgets,
/// or drain switch. Applies at that node's next heartbeat.
pub async fn put_node_runtime(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(node_id): axum::extract::Path<String>,
    Json(body): Json<PutNodeRuntime>,
) -> Result<Json<serde_json::Value>, Response> {
    let library = state.library_snapshot().await;
    let node_id = node_id.trim().to_string();
    bookclerk_library::control_plane::load_node(&library, &node_id)
        .await
        .map_err(events_error)?;
    let document = bookclerk_library::control_plane::update_node_runtime(
        &library,
        &operator_actor(),
        &node_id,
        |runtime| {
            if let Some(label) = &body.label {
                runtime.label.clone_from(label);
            }
            if let Some(percent) = body.cpu_budget_percent {
                runtime.cpu_budget_percent = percent;
            }
            if let Some(percent) = body.memory_budget_percent {
                runtime.memory_budget_percent = percent;
            }
            if let Some(schedulable) = body.schedulable {
                runtime.schedulable = schedulable;
            }
        },
    )
    .await
    .map_err(events_error)?;
    Ok(Json(json!({
        "node_id": node_id,
        "revision": document.revision,
        "runtime": document.body,
    })))
}

/// `PUT /api/config/nodes/{node_id}/labels` body.
#[derive(Debug, Deserialize)]
pub(crate) struct PutNodeLabels {
    /// The node's complete operator label set.
    labels: Vec<String>,
}

/// `PUT /api/config/nodes/{node_id}/labels`: replace a node's placement
/// labels. The scheduler re-plans on the wakeup this publishes.
pub async fn put_node_labels(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(node_id): axum::extract::Path<String>,
    Json(body): Json<PutNodeLabels>,
) -> Result<Json<serde_json::Value>, Response> {
    let cfg = state.config.read().await.clone();
    let this_node = this_node_id(&cfg)?;
    let library = state.library_snapshot().await;
    let node = bookclerk_library::control_plane::set_node_labels(
        &library,
        &operator_actor(),
        node_id.trim(),
        &body.labels,
    )
    .await
    .map_err(events_error)?;
    let statuses = node_statuses(&library).await?;
    let runtime = node_runtime_or_default(&library, &node.node_id).await?;
    Ok(Json(node_json(
        &node,
        &this_node,
        statuses.get(&node.node_id),
        &runtime,
    )))
}

/// `PUT /api/config/plugin-instances/{id}/placement` body.
#[derive(Debug, Deserialize)]
pub(crate) struct PutPluginPlacement {
    /// Nodes that should run the instance: a number (0 stops every
    /// replica) or `"all"`.
    replicas: bookclerk_library::control_plane::Replicas,
    /// Label constraints (`name`, `name=value`, `!name`).
    #[serde(default)]
    constraints: Vec<String>,
    /// Current revision to replace; omit (or 0) to create.
    #[serde(default)]
    expected_revision: Option<i64>,
}

/// `PUT /api/config/plugin-instances/{id}/placement`: hand the instance's
/// deployments to the scheduler with a replica count and constraints.
pub async fn put_plugin_placement_route(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<PutPluginPlacement>,
) -> Result<Json<serde_json::Value>, Response> {
    use bookclerk_library::control_plane::{
        put_plugin_placement, PlacementSpec, PlacementWrite, PluginInstanceId,
    };
    let instance_id = PluginInstanceId::parse(&id).map_err(events_error)?;
    let library = state.library_snapshot().await;
    let spec = PlacementSpec {
        replicas: body.replicas,
        constraints: body.constraints,
    };
    match put_plugin_placement(
        &library,
        &operator_actor(),
        &instance_id,
        &spec,
        body.expected_revision,
    )
    .await
    .map_err(events_error)?
    {
        PlacementWrite::Applied(placement) => Ok(Json(json!({
            "plugin_instance_id": instance_id.to_string(),
            "placement": PlacementView::from(placement),
        }))),
        PlacementWrite::Conflict { current_revision } => Err(conflict(current_revision)),
    }
}

/// `DELETE /api/config/plugin-instances/{id}/placement`: return the
/// instance to hand-managed enable/disable; its deployments stay as they
/// are.
pub async fn delete_plugin_placement_route(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, Response> {
    use bookclerk_library::control_plane::{delete_plugin_placement, PluginInstanceId};
    let instance_id = PluginInstanceId::parse(&id).map_err(events_error)?;
    let library = state.library_snapshot().await;
    let deleted = delete_plugin_placement(&library, &operator_actor(), &instance_id)
        .await
        .map_err(events_error)?;
    Ok(Json(json!({
        "plugin_instance_id": instance_id.to_string(),
        "deleted": deleted,
    })))
}

/// `GET /api/config/plugin-schemas`: every pinned settings schema.
pub async fn get_plugin_schemas(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, Response> {
    let library = state.library_snapshot().await;
    let schemas = bookclerk_library::control_plane::list_plugin_schemas(&library)
        .await
        .map_err(events_error)?;
    let rows: Vec<serde_json::Value> = schemas
        .into_iter()
        .map(|pinned| {
            json!({
                "plugin_key": pinned.plugin_key,
                "manifest_id": pinned.manifest_id,
                "schema_sha256": pinned.schema_sha256,
                "package_version": pinned.package_version,
                "recorded_at": pinned.recorded_at,
                "recorded_by_node": pinned.recorded_by_node,
                "settings": pinned.schema.settings,
            })
        })
        .collect();
    Ok(Json(json!({ "schemas": rows })))
}

/// 409 body carrying the revision that won.
fn conflict(current_revision: i64) -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({
            "error": "revision_conflict",
            "current_revision": current_revision,
        })),
    )
        .into_response()
}

/// Maps a library configuration error onto an HTTP status.
fn events_error(err: bookclerk_library::LibraryError) -> Response {
    let message = err.to_string();
    let status = if matches!(err, bookclerk_library::LibraryError::Conflict(_)) {
        StatusCode::CONFLICT
    } else if message.contains("unauthorized configuration write") {
        StatusCode::FORBIDDEN
    } else if message.contains("unsupported configuration schema")
        || message.contains("invalid configuration")
    {
        StatusCode::BAD_REQUEST
    } else if message.contains("not initialized") || message.contains("not found") {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (
        status,
        Json(json!({
            "error": message,
        })),
    )
        .into_response()
}

/// In-memory daemon state for control-plane publication tests.
#[cfg(test)]
pub(crate) fn control_plane_test_state(
    store: LibraryStore,
    config: bookclerk_config::Config,
) -> AppState {
    use std::sync::OnceLock;

    use tokio::sync::{Mutex, Notify, RwLock, Semaphore};

    AppState {
        config: Arc::new(RwLock::new(config)),
        library: Arc::new(RwLock::new(store)),
        database_registry: Arc::new(RwLock::new(
            bookclerk_plugin_host::DatabaseRegistry::default(),
        )),
        job_notify: Arc::new(Notify::new()),
        job_runtime: Arc::new(RwLock::new(())),
        work_lock: Mutex::new(()),
        discover_gate: Arc::new(Semaphore::new(1)),
        integrations: Arc::new(RwLock::new(
            bookclerk_integrations::IntegrationRegistry::new(),
        )),
        sources: Arc::new(RwLock::new(bookclerk_source::SourceRegistry::new())),
        destinations: Arc::new(RwLock::new(
            bookclerk_plugin_host::DestinationRegistry::default(),
        )),
        auth: Arc::new(RwLock::new(Arc::new(crate::auth::OperatorAuthState::new(
            "test-token".into(),
            1,
            false,
            1,
            1,
        )))),
        reload_lock: Mutex::new(()),
        listen_reload: Arc::new(Notify::new()),
        last_bound_listen: RwLock::new(None),
        tray: RwLock::new(None),
        tray_handoff: Mutex::new(None),
        deployment_runtime: OnceLock::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        control_plane_test_state, finish_reload_events, load_events_publication, publish_loaded,
        reconcile_events, EventsPublish, LoadedEvents,
    };
    use bookclerk_config::Config;
    use bookclerk_library::control_plane::{
        bootstrap_control_plane, overlay_events, replace_events, ConfigActor, EventsReplace,
        EventsSettingsV1,
    };
    use bookclerk_library::LibraryStore;

    /// Stops `pid` without the `kill` binary, which Windows images do not ship.
    #[allow(unsafe_code)]
    fn kill_pid(pid: u32) {
        #[cfg(unix)]
        {
            let rc = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            if rc != 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::ESRCH) {
                    panic!("kill {pid}: {err}");
                }
            }
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::Threading::{
                OpenProcess, TerminateProcess, PROCESS_TERMINATE,
            };
            unsafe {
                let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
                if handle.is_null() {
                    return;
                }
                let _ = TerminateProcess(handle, 1);
                let _ = CloseHandle(handle);
            }
        }
    }

    fn operator() -> ConfigActor {
        ConfigActor::Operator {
            id: "operator".into(),
        }
    }

    fn events(retention: u64) -> EventsSettingsV1 {
        EventsSettingsV1 {
            retention_days: retention,
            dead_letter_retention_days: 30,
            concurrency: 1,
        }
    }

    async fn enrolled() -> (
        LibraryStore,
        bookclerk_library::control_plane::ControlPlaneSession,
        Config,
        tempfile::TempDir,
    ) {
        let db = bookclerk_plugin_database_sqlite::open_memory()
            .await
            .expect("sqlite");
        bookclerk_library::apply_host_schema(&db)
            .await
            .expect("schema");
        let store = LibraryStore::from_connection(db);
        let files = tempfile::tempdir().expect("files");
        let mut config = Config::default();
        let scratch = files.path().join("cache");
        let session = bootstrap_control_plane(&store, files.path(), &scratch, None, &config.events)
            .await
            .expect("bootstrap");
        overlay_events(&mut config, &session.events, &session.cluster_id);
        (store, session, config, files)
    }

    #[tokio::test]
    async fn stale_read_cannot_replace_a_newer_publication() {
        let (store, session, config, _files) = enrolled().await;
        let state = control_plane_test_state(store.clone(), config);
        let stale = load_events_publication(&store).await.unwrap();
        assert_eq!(stale.document.revision, 1);

        let applied = replace_events(&store, &operator(), 1, &events(9), "daemon-pub-2")
            .await
            .unwrap();
        let EventsReplace::Applied(doc) = applied else {
            panic!("newer write should apply: {applied:?}");
        };
        assert!(
            publish_loaded(
                &state,
                &LoadedEvents {
                    cluster_id: session.cluster_id.clone(),
                    document: doc.clone(),
                },
                EventsPublish::Monotonic,
            )
            .await
        );

        assert!(
            !publish_loaded(&state, &stale, EventsPublish::Monotonic).await,
            "releasing the older read must not publish"
        );
        {
            let live = state.config.read().await;
            assert_eq!(live.events.retention_days, 9);
            assert_eq!(live.events_revision, Some(doc.revision));
            assert_eq!(
                live.events_authority.as_deref(),
                Some(session.cluster_id.as_str())
            );
        }

        let newer = replace_events(
            &store,
            &operator(),
            doc.revision,
            &events(10),
            "daemon-pub-3",
        )
        .await
        .unwrap();
        let EventsReplace::Applied(doc3) = newer else {
            panic!("third write should apply: {newer:?}");
        };
        let held = LoadedEvents {
            cluster_id: session.cluster_id.clone(),
            document: doc.clone(),
        };
        assert!(
            publish_loaded(
                &state,
                &LoadedEvents {
                    cluster_id: session.cluster_id.clone(),
                    document: doc3.clone(),
                },
                EventsPublish::Monotonic,
            )
            .await
        );
        assert!(!publish_loaded(&state, &held, EventsPublish::Monotonic).await);
        reconcile_events(&state).await.unwrap();
        let live = state.config.read().await;
        assert_eq!(live.events.retention_days, 10);
        assert_eq!(live.events_revision, Some(doc3.revision));
    }

    #[tokio::test]
    async fn database_swap_replaces_authority_and_stale_revisions_do_not() {
        let (store, session, config, _files) = enrolled().await;
        let state = control_plane_test_state(store.clone(), config);
        let original = load_events_publication(&store).await.unwrap();
        let mut swapped_doc = original.document.clone();
        swapped_doc.body = events(3);
        swapped_doc.revision = 1;
        let swapped = LoadedEvents {
            cluster_id: "other-cluster".into(),
            document: swapped_doc,
        };
        assert!(
            !publish_loaded(&state, &swapped, EventsPublish::Monotonic).await,
            "a different cluster is not a newer revision"
        );
        {
            let live = state.config.read().await;
            assert_eq!(
                live.events_authority.as_deref(),
                Some(session.cluster_id.as_str())
            );
            assert_eq!(live.events.retention_days, 7);
        }
        assert!(publish_loaded(&state, &swapped, EventsPublish::DatabaseSwap).await);
        assert!(!publish_loaded(&state, &original, EventsPublish::Monotonic).await);
        let live = state.config.read().await;
        assert_eq!(live.events_authority.as_deref(), Some("other-cluster"));
        assert_eq!(live.events.retention_days, 3);
        assert_eq!(live.events_revision, Some(1));
    }

    #[tokio::test]
    async fn reload_merge_keeps_the_newer_body_until_a_swap() {
        let (store, session, config, _files) = enrolled().await;
        let state = control_plane_test_state(store.clone(), config);
        let early = load_events_publication(&store).await.unwrap();
        let applied = replace_events(&store, &operator(), 1, &events(9), "reload-2")
            .await
            .unwrap();
        let EventsReplace::Applied(doc) = applied else {
            panic!("{applied:?}");
        };
        publish_loaded(
            &state,
            &LoadedEvents {
                cluster_id: session.cluster_id.clone(),
                document: doc,
            },
            EventsPublish::Monotonic,
        )
        .await;
        let live = state.config.read().await.clone();
        let mut candidate = Config::default();
        candidate.events.retention_days = 1;
        finish_reload_events(&live, &mut candidate, &early, false);
        assert_eq!(candidate.events.retention_days, 9);
        assert_eq!(candidate.events_revision, live.events_revision);
        assert_eq!(candidate.events_authority, live.events_authority);

        let mut other = early.document.clone();
        other.body = events(4);
        other.revision = 1;
        let fresh = LoadedEvents {
            cluster_id: "swapped-cluster".into(),
            document: other,
        };
        let swapped_from = candidate.clone();
        finish_reload_events(&swapped_from, &mut candidate, &fresh, true);
        assert_eq!(candidate.events.retention_days, 4);
        assert_eq!(candidate.events_revision, Some(1));
        assert_eq!(
            candidate.events_authority.as_deref(),
            Some("swapped-cluster")
        );
    }

    /// `[[settings]]` schema of the GraphicAudio manifest at `toml`.
    fn ga_schema(toml: &std::path::Path) -> bookclerk_plugin_abi::SettingsSchema {
        bookclerk_plugin_host::PluginManifest::parse(&std::fs::read_to_string(toml).unwrap())
            .unwrap()
            .settings_schema()
    }

    fn stage_graphicaudio_package(files_path: &std::path::Path, toml: &std::path::Path) -> String {
        stage_plugin_package(
            files_path,
            "graphicaudio",
            toml,
            "bookclerk-plugin-source-graphicaudio",
            "source",
            "graphicaudio",
            "outbound",
        )
    }

    fn stage_plugin_package(
        files_path: &std::path::Path,
        package_name: &str,
        toml: &std::path::Path,
        binary_name: &str,
        kind: &str,
        id: &str,
        network: &str,
    ) -> String {
        use std::path::PathBuf;

        let package_dir = files_path
            .join(bookclerk_plugin_host::AUTHORIZED_PACKAGE_DIR)
            .join(package_name);
        std::fs::create_dir_all(&package_dir).unwrap();
        let staging = files_path.join(format!("{package_name}-stage"));
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::copy(toml, staging.join("plugin.toml")).unwrap();
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| workspace.join("target"));
        let profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        let exe_name = format!("{binary_name}{}", std::env::consts::EXE_SUFFIX);
        let binary = target.join(profile).join(&exe_name);
        assert!(binary.is_file(), "missing {}", binary.display());
        if !std::env::consts::EXE_SUFFIX.is_empty() {
            let text = std::fs::read_to_string(staging.join("plugin.toml")).unwrap();
            std::fs::write(
                staging.join("plugin.toml"),
                text.replace(&format!("./{binary_name}"), &format!("./{exe_name}")),
            )
            .unwrap();
        }
        let staged_bin = staging.join(&exe_name);
        if std::fs::hard_link(&binary, &staged_bin).is_err() {
            std::fs::copy(&binary, &staged_bin).unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&staged_bin).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&staged_bin, perms).unwrap();
        }
        let archive = package_dir.join("archive.tar.gz");
        let tar = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&staging)
            .args(["plugin.toml", exe_name.as_str()])
            .status()
            .expect("tar");
        assert!(tar.success());
        let archive = archive.canonicalize().unwrap();
        let manifest = serde_json::json!({
            "schema_version": 1,
            "api_version": 1,
            "kind": kind,
            "id": id,
            "sandbox": { "network": network },
            "artifacts": [{
                "target": bookclerk_plugin_host::host_bookclerk_target(),
                "url": format!("file://{}", archive.display()),
                "archive_sha256": "ab".repeat(32),
                "executable": exe_name
            }]
        });
        std::fs::write(
            package_dir.join("package.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let packages =
            bookclerk_plugin_host::load_authorized_local_packages(files_path).expect("packages");
        let needle = format!("{package_name}/archive.tar.gz");
        let encoded = format!("{package_name}%2Farchive.tar.gz");
        packages
            .keys()
            .find(|key| key.contains(&needle) || key.contains(&encoded))
            .cloned()
            .unwrap_or_else(|| panic!("package key for {package_name} missing from {packages:?}"))
    }

    #[tokio::test]
    async fn daemon_reconcile_publishes_and_runs_a_local_archive_release() {
        use std::collections::BTreeMap;
        use std::path::PathBuf;

        use bookclerk_config::{EventsConfig, Isolation};
        use bookclerk_library::control_plane::{
            bootstrap_control_plane, create_plugin_instance, ensure_plugin_deployment,
            import_instance_config_if_absent, load_deployment, load_observation, ConfigActor,
            DeploymentStatus, DesiredState, PluginInstanceConfigV1, SettingValue,
        };

        let files = tempfile::tempdir().expect("files");
        let files_path = files.path();
        std::fs::write(files_path.join("config.toml"), "").unwrap();
        let toml = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../bookclerk-plugins/optional/source-graphicaudio/plugin.toml");
        let plugin_key = stage_graphicaudio_package(files_path, &toml);
        let plugin_key_for_lookup = plugin_key.clone();

        let db = bookclerk_plugin_database_sqlite::open(&files_path.join("library.db"))
            .await
            .expect("sqlite");
        bookclerk_library::apply_host_schema(&db)
            .await
            .expect("schema");
        let store = bookclerk_library::LibraryStore::from_connection(db);
        let scratch = files_path.join("cache");
        let session =
            bootstrap_control_plane(&store, files_path, &scratch, None, &EventsConfig::default())
                .await
                .expect("bootstrap");
        let actor = ConfigActor::Bootstrap;
        let instance = create_plugin_instance(&store, &actor, &plugin_key, "")
            .await
            .expect("instance");
        let mut settings = BTreeMap::new();
        settings.insert("access".into(), SettingValue::String("device".into()));
        import_instance_config_if_absent(
            &store,
            &actor,
            &instance.id,
            &ga_schema(&toml),
            &PluginInstanceConfigV1 {
                settings,
                secret_refs: Vec::new(),
            },
            "import-daemon-ga",
        )
        .await
        .expect("import");
        let toml_text = std::fs::read_to_string(&toml).unwrap();
        let manifest = bookclerk_plugin_host::PluginManifest::parse(&toml_text).unwrap();
        let mut grant = bookclerk_plugin_host::consent_request_alias(&manifest);
        grant.plugin_key = plugin_key;
        let mut grants = bookclerk_plugin_host::PluginGrantStore::default();
        grants.upsert(grant);
        grants.save(files_path).unwrap();
        let deployment = ensure_plugin_deployment(
            &store,
            &actor,
            &instance.id,
            &session.node.node_id,
            DesiredState::Present,
        )
        .await
        .expect("deployment");

        let mut config = Config::load(
            Some(files_path.to_path_buf()),
            Some(files_path.join("config.toml")),
        )
        .unwrap();
        config.plugins.isolation = Isolation::Off;
        let state = control_plane_test_state(store.clone(), config);
        super::reconcile_deployments(&state)
            .await
            .expect("daemon reconcile");

        let obs = load_observation(&store, &deployment.deployment_id, &session.node.node_id)
            .await
            .unwrap()
            .expect("observation");
        assert_eq!(obs.status, DeploymentStatus::Healthy, "{}", obs.detail);
        let desired = load_deployment(&store, &deployment.deployment_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(desired.desired, DesiredState::Present);
        // The operator package became the active cluster release; the node
        // runs it from the digest cache and wrote no receipt or ledger row.
        let release =
            bookclerk_library::control_plane::load_active_release(&store, &instance.plugin_key)
                .await
                .unwrap()
                .expect("active release");
        let artifact = bookclerk_plugin_host::host_artifact(&release).unwrap();
        assert!(
            artifact.url.starts_with("file://"),
            "a guest binary over the blob cap is referenced in place: {}",
            artifact.url
        );
        assert!(bookclerk_plugin_host::release_cache_path(
            files_path,
            &artifact.payload_root_sha256
        )
        .join("plugin.toml")
        .is_file());
        for entry in std::fs::read_dir(files_path.join("plugins")).unwrap() {
            let path = entry.unwrap().path();
            assert!(
                !path.join("receipt.json").is_file(),
                "no host-local install was committed at {}",
                path.display()
            );
        }
        assert!(!files_path.join("install-ledger.json").exists());

        let runtime = state.deployment_runtime();
        let instance_id = instance.id.as_str();
        let first_pid = runtime.tracked_guest_pid(instance_id).expect("guest pid");
        super::reconcile_deployments(&state)
            .await
            .expect("second tick");
        assert_eq!(runtime.tracked_guest_pid(instance_id), Some(first_pid));
        assert!(
            bookclerk_plugin_host::DeploymentRuntime::guest_still_running(
                runtime.as_ref(),
                instance_id
            )
            .await
        );
        let job_registry = crate::registry::registry_for_job(&state).await;
        let found = job_registry.get(instance_id).expect("instance address");
        assert_eq!(found.plugin_instance_id(), Some(instance_id));
        assert_eq!(found.guest_pid(), Some(first_pid));
        assert!(job_registry.get(&plugin_key_for_lookup).is_some());

        kill_pid(first_pid);
        let mut dead = false;
        for _ in 0..50 {
            if !bookclerk_plugin_host::DeploymentRuntime::guest_still_running(
                runtime.as_ref(),
                instance_id,
            )
            .await
            {
                dead = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(dead, "killed guest still looks alive");
        super::reconcile_deployments(&state)
            .await
            .expect("replacement tick");
        let second_pid = runtime
            .tracked_guest_pid(instance_id)
            .expect("replacement pid");
        assert_ne!(first_pid, second_pid);
        assert!(
            bookclerk_plugin_host::DeploymentRuntime::guest_still_running(
                runtime.as_ref(),
                instance_id
            )
            .await
        );
        kill_pid(second_pid);
    }

    #[tokio::test]
    async fn daemon_job_lookup_reaches_both_instances_of_one_key() {
        use std::collections::BTreeMap;
        use std::path::PathBuf;

        use bookclerk_config::{EventsConfig, Isolation};
        use bookclerk_library::control_plane::{
            bootstrap_control_plane, create_plugin_instance, ensure_plugin_deployment,
            import_instance_config_if_absent, load_observation, ConfigActor, DeploymentStatus,
            DesiredState, PluginInstanceConfigV1, SettingValue,
        };

        let files = tempfile::tempdir().expect("files");
        let files_path = files.path();
        std::fs::write(files_path.join("config.toml"), "").unwrap();
        let toml = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../bookclerk-plugins/optional/source-graphicaudio/plugin.toml");
        let plugin_key = stage_graphicaudio_package(files_path, &toml);

        let db = bookclerk_plugin_database_sqlite::open(&files_path.join("library.db"))
            .await
            .expect("sqlite");
        bookclerk_library::apply_host_schema(&db)
            .await
            .expect("schema");
        let store = bookclerk_library::LibraryStore::from_connection(db);
        let scratch = files_path.join("cache");
        let session =
            bootstrap_control_plane(&store, files_path, &scratch, None, &EventsConfig::default())
                .await
                .expect("bootstrap");
        let actor = ConfigActor::Bootstrap;
        let first = create_plugin_instance(&store, &actor, &plugin_key, "")
            .await
            .expect("first instance");
        let second = create_plugin_instance(&store, &actor, &plugin_key, "")
            .await
            .expect("second instance");
        assert_ne!(first.id, second.id);
        let mut deployment_ids = Vec::new();
        for (instance, base_url, operation) in [
            (&first, "http://alpha.example", "import-alpha"),
            (&second, "http://beta.example", "import-beta"),
        ] {
            let mut settings = BTreeMap::new();
            settings.insert("access".into(), SettingValue::String("device".into()));
            settings.insert("base_url".into(), SettingValue::String(base_url.into()));
            import_instance_config_if_absent(
                &store,
                &actor,
                &instance.id,
                &ga_schema(&toml),
                &PluginInstanceConfigV1 {
                    settings,
                    secret_refs: Vec::new(),
                },
                operation,
            )
            .await
            .expect("import");
            let deployment = ensure_plugin_deployment(
                &store,
                &actor,
                &instance.id,
                &session.node.node_id,
                DesiredState::Present,
            )
            .await
            .expect("deployment");
            deployment_ids.push(deployment.deployment_id);
        }
        let toml_text = std::fs::read_to_string(&toml).unwrap();
        let manifest = bookclerk_plugin_host::PluginManifest::parse(&toml_text).unwrap();
        let mut grant = bookclerk_plugin_host::consent_request_alias(&manifest);
        grant.plugin_key = plugin_key.clone();
        let mut grants = bookclerk_plugin_host::PluginGrantStore::default();
        grants.upsert(grant);
        grants.save(files_path).unwrap();

        let mut config = Config::load(
            Some(files_path.to_path_buf()),
            Some(files_path.join("config.toml")),
        )
        .unwrap();
        config.plugins.isolation = Isolation::Off;
        let state = control_plane_test_state(store.clone(), config);
        super::reconcile_deployments(&state)
            .await
            .expect("daemon reconcile");
        for deployment_id in &deployment_ids {
            let obs = load_observation(&store, deployment_id, &session.node.node_id)
                .await
                .unwrap()
                .expect("observation");
            assert_eq!(obs.status, DeploymentStatus::Healthy, "{}", obs.detail);
        }

        let runtime = state.deployment_runtime();
        let registry = crate::registry::registry_for_job(&state).await;
        for (instance, base_url) in [
            (&first, "http://alpha.example"),
            (&second, "http://beta.example"),
        ] {
            let id = instance.id.as_str();
            let source = registry.get(id).expect("job lookup");
            assert_eq!(source.plugin_instance_id(), Some(id));
            assert_eq!(source.guest_pid(), runtime.tracked_guest_pid(id));
            let opened = runtime.opened_config_json(id).expect("opened config");
            assert_eq!(opened["base_url"], base_url);
            assert_eq!(opened["access"], "device");
        }
        let routed = registry
            .get(&plugin_key)
            .expect("a bare plugin key routes to the default instance");
        assert_eq!(routed.plugin_instance_id(), Some(first.id.as_str()));
        assert_eq!(
            registry
                .get("graphicaudio")
                .expect("alias routes")
                .plugin_instance_id(),
            Some(first.id.as_str())
        );
        let pinned = registry
            .route(&plugin_key, Some(second.id.as_str()))
            .expect("a job pin selects the other instance");
        assert_eq!(pinned.plugin_instance_id(), Some(second.id.as_str()));
        let unknown = registry
            .require_routed(&plugin_key, Some("not-an-instance"))
            .err()
            .expect("an unknown pin does not fall back")
            .to_string();
        assert!(unknown.contains("not a registered instance"), "{unknown}");

        for instance in [&first, &second] {
            if let Some(pid) = runtime.tracked_guest_pid(instance.id.as_str()) {
                kill_pid(pid);
            }
        }
    }

    #[tokio::test]
    async fn daemon_reload_keeps_deployed_sessions() {
        use std::path::PathBuf;

        use bookclerk_config::Isolation;
        use bookclerk_library::control_plane::{
            bootstrap_control_plane, create_plugin_instance, ensure_plugin_deployment,
            import_instance_config_if_absent, load_observation, ConfigActor, DeploymentStatus,
            DesiredState, PluginInstanceConfigV1, SettingValue,
        };

        let files = tempfile::tempdir().expect("files");
        let files_path = files.path();
        std::fs::write(
            files_path.join("config.toml"),
            "[plugins]\nisolation = \"off\"\n\n[daemon.auth]\nenabled = false\n",
        )
        .unwrap();
        let toml = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../bookclerk-plugins/optional/source-graphicaudio/plugin.toml");
        let plugin_key = stage_graphicaudio_package(files_path, &toml);

        let db = bookclerk_plugin_database_sqlite::open(&files_path.join("library.db"))
            .await
            .expect("sqlite");
        bookclerk_library::apply_host_schema(&db)
            .await
            .expect("schema");
        let store = bookclerk_library::LibraryStore::from_connection(db);
        let scratch = files_path.join("cache");
        let session = bootstrap_control_plane(
            &store,
            files_path,
            &scratch,
            None,
            &bookclerk_config::EventsConfig::default(),
        )
        .await
        .expect("bootstrap");
        let actor = ConfigActor::Bootstrap;
        let instance = create_plugin_instance(&store, &actor, &plugin_key, "")
            .await
            .expect("instance");
        let mut settings = std::collections::BTreeMap::new();
        settings.insert("access".into(), SettingValue::String("device".into()));
        import_instance_config_if_absent(
            &store,
            &actor,
            &instance.id,
            &ga_schema(&toml),
            &PluginInstanceConfigV1 {
                settings,
                secret_refs: Vec::new(),
            },
            "import-reload-ga",
        )
        .await
        .expect("import");
        let manifest =
            bookclerk_plugin_host::PluginManifest::parse(&std::fs::read_to_string(&toml).unwrap())
                .unwrap();
        let mut grant = bookclerk_plugin_host::consent_request_alias(&manifest);
        grant.plugin_key = plugin_key.clone();
        let mut grants = bookclerk_plugin_host::PluginGrantStore::default();
        grants.upsert(grant);
        grants.save(files_path).unwrap();
        let deployment = ensure_plugin_deployment(
            &store,
            &actor,
            &instance.id,
            &session.node.node_id,
            DesiredState::Present,
        )
        .await
        .expect("deployment");

        let config = Config::load(
            Some(files_path.to_path_buf()),
            Some(files_path.join("config.toml")),
        )
        .unwrap();
        assert_eq!(config.plugins.isolation, Isolation::Off);
        let state = control_plane_test_state(store.clone(), config);
        super::reconcile_deployments(&state)
            .await
            .expect("daemon reconcile");
        let obs = load_observation(&store, &deployment.deployment_id, &session.node.node_id)
            .await
            .unwrap()
            .expect("observation");
        assert_eq!(obs.status, DeploymentStatus::Healthy, "{}", obs.detail);

        let runtime = state.deployment_runtime();
        let instance_id = instance.id.as_str();
        let pid = runtime.tracked_guest_pid(instance_id).expect("guest pid");

        // Reload re-enrolls local plugins and runs a reconcile pass; the
        // healthy guest the reconciler owns is neither stopped nor respawned.
        crate::api::reload_daemon_config(&state)
            .await
            .expect("reload");
        assert_eq!(runtime.tracked_guest_pid(instance_id), Some(pid));
        let after = crate::registry::registry_for_job(&state).await;
        let by_instance = after.get(instance_id).expect("job lookup after reload");
        assert_eq!(by_instance.guest_pid(), Some(pid));
        let by_key = after
            .get(&plugin_key)
            .expect("plugin key routes after reload");
        assert_eq!(by_key.plugin_instance_id(), Some(instance_id));
        assert!(
            bookclerk_plugin_host::DeploymentRuntime::guest_still_running(
                runtime.as_ref(),
                instance_id
            )
            .await
        );

        // Re-enrollment adopted the existing instance rather than minting one.
        let instances =
            crate::config_authority::list_plugin_instance_views(&store, &session.node.node_id)
                .await
                .expect("instance views");
        let ours: Vec<_> = instances
            .iter()
            .filter(|view| view.plugin_key == plugin_key)
            .collect();
        assert_eq!(ours.len(), 1, "{instances:?}");
        assert!(ours[0].is_default);
        assert_eq!(ours[0].alias, "graphicaudio");
        assert!(ours[0].schema.iter().any(|spec| spec.key == "access"));
        let local = ours[0]
            .deployments
            .iter()
            .find(|row| row.node_id == session.node.node_id)
            .expect("this node's deployment");
        assert_eq!(local.status.as_deref(), Some("healthy"));

        crate::api::reload_daemon_config(&state)
            .await
            .expect("second reload");
        assert_eq!(runtime.tracked_guest_pid(instance_id), Some(pid));

        kill_pid(pid);
    }
}
