//! Runtime application of database-backed `[events]` and `[output]`
//! configuration.
//!
//! The dispatcher and pruner read [`bookclerk_config::Config::events`] on each
//! tick, and every acquire, convert, and packaging pass reads
//! [`bookclerk_config::Config::output`] when it starts. This module publishes
//! committed `core.events` and `core.output` revisions into those structs.
//! Local delivery-task count is chosen when the event runtime starts and stays
//! at that count until process restart; retention, the in-flight cap, and
//! every output setting follow the latest applied revision.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Extension;
use axum::Json;
use bookclerk_library::control_plane::{
    load_cluster_row, load_events, load_output, overlay_events, overlay_output, replace_events,
    replace_output, ConfigActor, ConfigurationDocument, EventsReplace, EventsSettingsV1,
    OutputReplace, OutputSettingsV1, EVENTS_NAMESPACE,
};
use bookclerk_library::LibraryStore;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::api::AppState;
use crate::auth::NodeAdminCaller;

/// Operator actor recorded on configuration commits from the daemon.
fn operator_actor() -> ConfigActor {
    ConfigActor::Operator {
        id: "operator".into(),
    }
}

/// Re-reads `core.events` and `core.output` on each `configuration.changed`
/// delivery on this node's queue, plus a slow backstop pass, until the
/// process stops.
pub fn spawn_config_reconciler(state: Arc<AppState>) {
    crate::node_queue::spawn_woken_loop(
        state,
        crate::node_queue::config_wait,
        |state| async move {
            if let Err(err) = reconcile_events(&state).await {
                tracing::warn!(error = %err, "configuration reconcile failed");
            }
            if let Err(err) = reconcile_output(&state).await {
                tracing::warn!(error = %err, "core.output reconcile failed");
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
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct PutEventsDomainRequest {
    /// Revision the caller read. A mismatch is a conflict.
    #[schema(example = 12)]
    pub expected_revision: i64,
    /// Replacement `[events]` document.
    #[schema(example = 30)]
    pub retention_days: u64,
    /// Replacement dead-letter retention.
    #[schema(example = 90)]
    pub dead_letter_retention_days: u64,
    /// Replacement concurrency cap.
    #[schema(example = 4)]
    pub concurrency: u32,
}

/// `GET /api/config/domains/core.events`.
#[utoipa::path(
    get,
    path = "/api/config/domains/core.events",
    responses(
        (status = 200, description = "The `core.events` settings.", body = EventsDomainResponse),
        (status = 500, description = "The configuration could not be read."),
    ),
)]
pub async fn get_events_domain(
    State(state): State<Arc<AppState>>,
) -> Result<Json<EventsDomainResponse>, Response> {
    let library = state.library_snapshot().await;
    let doc = load_events(&library).await.map_err(events_error)?;
    Ok(Json(EventsDomainResponse::from_document(&doc)))
}

/// `PUT /api/config/domains/core.events`.
#[utoipa::path(
    put,
    path = "/api/config/domains/core.events",
    request_body = PutEventsDomainRequest,
    responses(
        (status = 200, description = "The settings after the change, or a `replayed` note when the same change had already been written.", body = EventsDomainChange),
        (status = 400, description = "The body is invalid."),
        (status = 409, description = "`expected_revision` is stale; `current_revision` is the revision to retry against.", body = RevisionConflict),
        (status = 500, description = "The configuration could not be read or written."),
    ),
)]
pub async fn put_events_domain(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PutEventsDomainRequest>,
) -> Result<Json<EventsDomainChange>, Response> {
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
            Ok(Json(EventsDomainChange::Applied(
                EventsDomainResponse::from_document(&doc),
            )))
        }
        EventsReplace::Replayed { revision } => Ok(Json(EventsDomainChange::Replayed(
            DomainReplayed::new(EVENTS_NAMESPACE, revision),
        ))),
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
        events_error(bookclerk_library::LibraryError::NotFound(
            "cluster identity is not initialized".into(),
        ))
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

/// Where a configuration domain document lives and which revision it is at:
/// the metadata every domain response carries before its settings.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct DomainRevision {
    /// Domain namespace (`core.events` or `core.output`).
    #[schema(example = "core.events")]
    pub namespace: String,
    /// Scope type: `cluster` for a shared document, `node` for one node's.
    #[schema(example = "cluster")]
    pub scope_type: String,
    /// Scope id: `singleton` for the cluster, else the node id.
    #[schema(example = "singleton")]
    pub scope_id: String,
    /// Version of the document's schema, starting at 1.
    #[schema(example = 1)]
    pub schema_version: i64,
    /// Compare-and-swap revision; send it back as `expected_revision`.
    #[schema(example = 12)]
    pub revision: i64,
    /// RFC 3339 time of the last write.
    #[schema(example = "2026-10-28T19:07:00Z")]
    pub updated_at: String,
    /// Audit identity of the last writer.
    #[schema(example = "operator")]
    pub updated_by: String,
}

impl DomainRevision {
    /// The metadata of `doc`, whatever its body.
    fn of<T>(doc: &ConfigurationDocument<T>) -> Self {
        Self {
            namespace: doc.namespace.clone(),
            scope_type: doc.scope_type.clone(),
            scope_id: doc.scope_id.clone(),
            schema_version: doc.schema_version,
            revision: doc.revision,
            updated_at: doc.updated_at.clone(),
            updated_by: doc.updated_by.clone(),
        }
    }
}

/// Body of `GET /api/config/domains/core.events` and of a `PUT` that was
/// applied: the revision metadata, then the `[events]` settings themselves.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct EventsDomainResponse {
    /// Where the document lives and which revision it is at.
    #[serde(flatten)]
    pub revision: DomainRevision,
    /// The settings at that revision.
    #[serde(flatten)]
    pub settings: EventsSettingsV1,
}

impl EventsDomainResponse {
    /// Restates a stored document as the API body.
    fn from_document(doc: &ConfigurationDocument<EventsSettingsV1>) -> Self {
        Self {
            revision: DomainRevision::of(doc),
            settings: doc.body.clone(),
        }
    }
}

/// Body of a `PUT` to a configuration domain that changed nothing because
/// the same change (by its request id) had already been written.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct DomainReplayed {
    /// Domain namespace (`core.events` or `core.output`).
    #[schema(example = "core.events")]
    pub namespace: String,
    /// Always `true`: the write was a replay.
    #[schema(example = true)]
    pub replayed: bool,
    /// Revision the earlier write produced.
    #[schema(example = 12)]
    pub revision: i64,
}

impl DomainReplayed {
    /// A replay note for `namespace` at `revision`.
    fn new(namespace: &str, revision: i64) -> Self {
        Self {
            namespace: namespace.to_string(),
            replayed: true,
            revision,
        }
    }
}

/// Body of `PUT /api/config/domains/core.events`: the settings after the
/// change, or a replay note.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum EventsDomainChange {
    /// The change was written; these are the settings now.
    Applied(EventsDomainResponse),
    /// The same change had already been written.
    Replayed(DomainReplayed),
}

/// Body of a `409 Conflict` from a compare-and-swap write whose
/// `expected_revision` lost to a newer revision.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct RevisionConflict {
    /// Always `revision_conflict`.
    #[schema(example = "revision_conflict")]
    pub error: &'static str,
    /// The revision to read and retry against.
    #[schema(example = 13)]
    pub current_revision: i64,
}

/// One `core.output` read tied to the cluster that stored it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LoadedOutput {
    /// `cluster_identity.cluster_id` read with the document.
    pub cluster_id: String,
    /// Typed document and revision.
    pub document: ConfigurationDocument<OutputSettingsV1>,
}

/// Reads `core.output` and the cluster id that owns it.
///
/// # Errors
///
/// Returns an error when the cluster row or the document cannot be read. The
/// in-memory config is left unchanged.
pub(crate) async fn load_output_publication(
    library: &LibraryStore,
) -> anyhow::Result<LoadedOutput> {
    let row = load_cluster_row(library)
        .await?
        .ok_or_else(|| anyhow::anyhow!("cluster identity is not initialized"))?;
    let document = load_output(library).await?;
    Ok(LoadedOutput {
        cluster_id: row.cluster_id,
        document,
    })
}

/// Applies `loaded` onto `config` when `mode` allows it, under the same
/// monotonic, per-cluster rule as [`install_events`].
pub(crate) fn install_output(
    config: &mut bookclerk_config::Config,
    loaded: &LoadedOutput,
    mode: EventsPublish,
) -> bool {
    if mode == EventsPublish::Monotonic {
        if let Some(current) = config.output_authority.as_deref() {
            if current != loaded.cluster_id {
                return false;
            }
            if config
                .output_revision
                .is_some_and(|revision| loaded.document.revision <= revision)
            {
                return false;
            }
        }
    }
    overlay_output(config, &loaded.document, &loaded.cluster_id);
    true
}

/// Merges a freshly read output document into `candidate` without
/// regressing `live` (see [`finish_reload_events`]).
///
/// `candidate.output.in_progress` is host-local bootstrap from the reloaded
/// `config.toml` and is kept.
pub(crate) fn finish_reload_output(
    live: &bookclerk_config::Config,
    candidate: &mut bookclerk_config::Config,
    fresh: &LoadedOutput,
    database_swap: bool,
) {
    let in_progress = candidate.output.in_progress.clone();
    candidate.output = live.output.clone();
    candidate.output.in_progress = in_progress;
    candidate.output_revision = live.output_revision;
    candidate.output_authority = live.output_authority.clone();
    let mode = if database_swap
        || live
            .output_authority
            .as_deref()
            .is_some_and(|id| id != fresh.cluster_id)
    {
        EventsPublish::DatabaseSwap
    } else {
        EventsPublish::Monotonic
    };
    install_output(candidate, fresh, mode);
}

/// Publishes `loaded` under the config write lock.
pub(crate) async fn publish_loaded_output(
    state: &AppState,
    loaded: &LoadedOutput,
    mode: EventsPublish,
) -> bool {
    let mut config = state.config.write().await;
    let applied = install_output(&mut config, loaded, mode);
    if applied {
        tracing::info!(
            revision = loaded.document.revision,
            cluster_id = %loaded.cluster_id,
            "applied core.output from the database"
        );
    }
    applied
}

/// Applies a newer cluster output document onto the live config.
///
/// # Errors
///
/// Returns an error when the document cannot be read. The in-memory config is
/// left unchanged.
pub async fn reconcile_output(state: &AppState) -> anyhow::Result<()> {
    let library = state.library_snapshot().await;
    let loaded = load_output_publication(&library).await?;
    publish_loaded_output(state, &loaded, EventsPublish::Monotonic).await;
    Ok(())
}

/// JSON body for `PUT /api/config/domains/core.output`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct PutOutputDomainRequest {
    /// Revision the caller read. A mismatch is a conflict.
    #[schema(example = 12)]
    pub expected_revision: i64,
    /// Complete replacement document (schema version 1).
    pub document: OutputSettingsV1,
}

/// `GET /api/config/domains/core.output`.
#[utoipa::path(
    get,
    path = "/api/config/domains/core.output",
    responses(
        (status = 200, description = "The `core.output` settings.", body = OutputDomainResponse),
        (status = 500, description = "The configuration could not be read."),
    ),
)]
pub async fn get_output_domain(
    State(state): State<Arc<AppState>>,
) -> Result<Json<OutputDomainResponse>, Response> {
    let library = state.library_snapshot().await;
    let doc = load_output(&library).await.map_err(events_error)?;
    Ok(Json(OutputDomainResponse::from_document(&doc)))
}

/// `PUT /api/config/domains/core.output`.
#[utoipa::path(
    put,
    path = "/api/config/domains/core.output",
    request_body = PutOutputDomainRequest,
    responses(
        (status = 200, description = "The settings after the change, or a `replayed` note when the same change had already been written.", body = OutputDomainChange),
        (status = 400, description = "The body is invalid."),
        (status = 409, description = "`expected_revision` is stale; `current_revision` is the revision to retry against.", body = RevisionConflict),
        (status = 500, description = "The configuration could not be read or written."),
    ),
)]
pub async fn put_output_domain(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PutOutputDomainRequest>,
) -> Result<Json<OutputDomainChange>, Response> {
    let library = state.library_snapshot().await;
    let outcome = replace_output(
        &library,
        &operator_actor(),
        body.expected_revision,
        &body.document,
        &uuid::Uuid::new_v4().to_string(),
    )
    .await
    .map_err(events_error)?;
    match outcome {
        OutputReplace::Applied(doc) => {
            let cluster_id = cluster_id_of(&library).await?;
            publish_output(state.as_ref(), &doc, &cluster_id).await;
            Ok(Json(OutputDomainChange::Applied(Box::new(
                OutputDomainResponse::from_document(&doc),
            ))))
        }
        OutputReplace::Replayed { revision } => Ok(Json(OutputDomainChange::Replayed(
            DomainReplayed::new(bookclerk_library::control_plane::OUTPUT_NAMESPACE, revision),
        ))),
        OutputReplace::Conflict { current_revision } => Err(conflict(current_revision)),
    }
}

/// Applies `output.*` settings keys to `core.output` through compare-and-swap.
///
/// Every key is applied to one copy of the current document, which is then
/// validated and written as a single revision.
///
/// `expected_revision` is the revision the caller read. When it is given and
/// the stored document has moved on, nothing is written and the call is a
/// conflict, so a stale editor cannot overwrite a concurrent change.
///
/// # Errors
///
/// Returns 400 for unknown keys or invalid values and 409 when
/// `expected_revision` is stale or the revision lost to a concurrent write.
pub async fn commit_output_settings(
    state: &AppState,
    updates: &[(String, String)],
    expected_revision: Option<i64>,
) -> Result<(), Response> {
    if updates.is_empty() {
        return Ok(());
    }
    let library = state.library_snapshot().await;
    let current = load_output(&library).await.map_err(events_error)?;
    if expected_revision.is_some_and(|expected| expected != current.revision) {
        return Err(conflict(current.revision));
    }
    let mut body = current.body.clone();
    for (key, value) in updates {
        body.set(key, value).map_err(|err| {
            tracing::warn!(%key, error = %err, "rejected output settings update");
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": err.to_string() })),
            )
                .into_response()
        })?;
    }
    if body == current.body {
        return Ok(());
    }
    let outcome = replace_output(
        &library,
        &operator_actor(),
        current.revision,
        &body,
        &uuid::Uuid::new_v4().to_string(),
    )
    .await
    .map_err(events_error)?;
    match outcome {
        OutputReplace::Applied(doc) => {
            let cluster_id = cluster_id_of(&library).await?;
            publish_output(state, &doc, &cluster_id).await;
            Ok(())
        }
        OutputReplace::Replayed { .. } => Ok(()),
        OutputReplace::Conflict { current_revision } => Err(conflict(current_revision)),
    }
}

/// Copies a committed output document into the live daemon config.
async fn publish_output(
    state: &AppState,
    doc: &ConfigurationDocument<OutputSettingsV1>,
    cluster_id: &str,
) {
    let loaded = LoadedOutput {
        cluster_id: cluster_id.to_string(),
        document: doc.clone(),
    };
    publish_loaded_output(state, &loaded, EventsPublish::Monotonic).await;
}

/// Body of `GET /api/config/domains/core.output` and of a `PUT` that was
/// applied: the revision metadata and the `[output]` document.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct OutputDomainResponse {
    /// Where the document lives and which revision it is at.
    #[serde(flatten)]
    pub revision: DomainRevision,
    /// The settings at that revision (schema version 1).
    pub document: OutputSettingsV1,
}

impl OutputDomainResponse {
    /// Restates a stored document as the API body.
    fn from_document(doc: &ConfigurationDocument<OutputSettingsV1>) -> Self {
        Self {
            revision: DomainRevision::of(doc),
            document: doc.body.clone(),
        }
    }
}

/// Body of `PUT /api/config/domains/core.output`: the settings after the
/// change, or a replay note.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum OutputDomainChange {
    /// The change was written; these are the settings now (boxed: the
    /// output document is far larger than a replay note).
    Applied(Box<OutputDomainResponse>),
    /// The same change had already been written.
    Replayed(DomainReplayed),
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
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub(crate) struct PluginDeploymentView {
    /// Deployment row id.
    pub deployment_id: String,
    /// Node the row targets.
    pub node_id: String,
    /// What the row asks the node to do: `present` / `absent` from an
    /// operator, `placed` / `unplaced` from the scheduler.
    pub desired: bookclerk_library::control_plane::DesiredState,
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
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub(crate) struct PlacementView {
    /// Nodes that should run the instance.
    #[schema(schema_with = crate::openapi::replicas_schema)]
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
                desired: deployment.desired,
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
#[utoipa::path(
    get,
    path = "/api/config/plugin-instances",
    responses(
        (status = 200, description = "Every plugin instance with its deployments.", body = serde_json::Value),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
    ),
)]
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
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(crate) struct CreatePluginInstance {
    /// Canonical plugin key; the plugin must have a pinned schema.
    plugin_key: String,
    /// Operator-facing name.
    #[serde(default)]
    display_name: String,
    /// Make the instance local to this node (H-2): it deploys only there
    /// and is never placed by the scheduler.
    #[serde(default)]
    node_id: Option<String>,
}

/// `POST /api/config/plugin-instances`: a second (non-default) instance of
/// a plugin, with an empty document so schema defaults apply. With
/// `node_id`, the instance is local to that node.
#[utoipa::path(
    post,
    path = "/api/config/plugin-instances",
    request_body = CreatePluginInstance,
    responses(
        (status = 200, description = "The new instance.", body = serde_json::Value),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
        (status = 404, description = "No such plugin."),
        (status = 409, description = "The instance id is taken."),
    ),
)]
pub async fn post_plugin_instance(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreatePluginInstance>,
) -> Result<Json<serde_json::Value>, Response> {
    use bookclerk_library::control_plane::{
        create_node_local_instance, create_plugin_instance, import_instance_config_if_absent,
        require_plugin_schema, PluginInstanceConfigV1,
    };
    let library = state.library_snapshot().await;
    let pinned = require_plugin_schema(&library, body.plugin_key.trim())
        .await
        .map_err(events_error)?;
    let name = body.display_name.trim();
    let instance = match body.node_id.as_deref().map(str::trim) {
        Some(node) => {
            create_node_local_instance(&library, &operator_actor(), &pinned.plugin_key, node, name)
                .await
        }
        None => create_plugin_instance(&library, &operator_actor(), &pinned.plugin_key, name).await,
    }
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
        "owner_node_id": instance.owner_node_id,
    })))
}

/// `DELETE /api/config/plugin-instances/{id}`: removes an instance that has
/// no deployment left.
#[utoipa::path(
    delete,
    path = "/api/config/plugin-instances/{id}",
    params(("id" = String, Path, description = "Plugin instance id.")),
    responses(
        (status = 200, description = "The instance was removed.", body = serde_json::Value),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
        (status = 404, description = "No such instance."),
        (status = 409, description = "The instance is a default or still deployed."),
    ),
)]
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
#[utoipa::path(
    get,
    path = "/api/config/plugin-instances/{id}/config",
    params(("id" = String, Path, description = "Plugin instance id.")),
    responses(
        (status = 200, description = "The instance's settings document (secrets as presence flags).", body = serde_json::Value),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
        (status = 404, description = "No such instance."),
    ),
)]
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
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(crate) struct PutPluginInstanceConfig {
    /// Revision the caller last observed.
    expected_revision: i64,
    /// Scalar settings. Secret values are not accepted here.
    #[serde(default)]
    #[schema(value_type = Object)]
    settings: std::collections::BTreeMap<String, bookclerk_library::control_plane::SettingValue>,
    /// Secret refs to keep. Omitted refs are dropped from the document; the
    /// ciphertext stays sealed until the secret is overwritten.
    #[serde(default)]
    #[schema(value_type = Vec<Object>)]
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
#[utoipa::path(
    put,
    path = "/api/config/plugin-instances/{id}/config",
    params(("id" = String, Path, description = "Plugin instance id.")),
    request_body = PutPluginInstanceConfig,
    responses(
        (status = 200, description = "The document after the change.", body = serde_json::Value),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
        (status = 404, description = "No such instance."),
    ),
)]
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
            events_error(bookclerk_library::LibraryError::Invalid(format!(
                "invalid configuration: {err}"
            )))
        })?;
        if plaintext.is_empty() {
            return Err(events_error(bookclerk_library::LibraryError::Invalid(
                format!("invalid configuration: secret `{key}` is empty"),
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
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(crate) struct PutPluginDeployment {
    /// What the node should do: `present` or `absent` (the scheduler's
    /// `placed` / `unplaced` are accepted too).
    #[schema(value_type = bookclerk_library::control_plane::DesiredState)]
    desired: String,
    /// Revision the caller last observed; required when the row exists and
    /// the caller wants compare-and-swap. Omitted means "replace whatever is
    /// current".
    #[serde(default)]
    expected_revision: Option<i64>,
}

/// `PUT /api/config/plugin-instances/{id}/deployments/{node_id}`: the
/// Deployed switch. Creates the row when the node has none.
#[utoipa::path(
    put,
    path = "/api/config/plugin-instances/{id}/deployments/{node_id}",
    params(("id" = String, Path, description = "Plugin instance id."), ("node_id" = String, Path, description = "Node id.")),
    request_body = PutPluginDeployment,
    responses(
        (status = 200, description = "The deployment after the change.", body = serde_json::Value),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
        (status = 404, description = "No such instance."),
        (status = 409, description = "The instance is placed by the scheduler."),
    ),
)]
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
    let (existing, expected_revision, created) = match existing {
        Some(current) => (current, body.expected_revision, false),
        None => {
            // `ensure` returns a row another writer (enrollment, a second
            // request) created first, whatever its state. When that state is
            // not the one asked for, replace it against its revision rather
            // than report it as applied.
            let created = ensure_plugin_deployment(
                &library,
                &operator_actor(),
                &instance_id,
                &node_id,
                desired,
            )
            .await
            .map_err(events_error)?;
            let revision = created.revision;
            (created, Some(revision), true)
        }
    };
    let row = match existing {
        current if current.desired == desired && (created || body.expected_revision.is_none()) => {
            current
        }
        current => {
            let expected = expected_revision.unwrap_or(current.revision);
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

/// Nodes `caller` may see: every node for an operator, the owned ones for
/// a node owner.
async fn visible_nodes(
    library: &LibraryStore,
    caller: NodeAdminCaller,
) -> Result<Vec<bookclerk_library::control_plane::NodeRecord>, Response> {
    match caller {
        NodeAdminCaller::Operator => bookclerk_library::control_plane::list_nodes(library)
            .await
            .map_err(events_error),
        NodeAdminCaller::NodeOwner { user_id } => {
            bookclerk_library::control_plane::nodes_owned_by(library, user_id)
                .await
                .map_err(events_error)
        }
    }
}

/// JSON for `nodes` as `caller` sees them.
async fn nodes_json(
    library: &LibraryStore,
    this_node: &str,
    nodes: &[bookclerk_library::control_plane::NodeRecord],
) -> Result<Vec<serde_json::Value>, Response> {
    let statuses = node_statuses(library).await?;
    let mut out = Vec::with_capacity(nodes.len());
    for node in nodes {
        let runtime = node_runtime_or_default(library, &node.node_id).await?;
        let owner = match node.owner_user_id {
            Some(user_id) => library
                .get_user(user_id)
                .await
                .map_err(events_error)?
                .map(|user| {
                    json!({
                        "user_id": user.id,
                        "display_name": user.display_name.or(user.login_name),
                        "role": user.role.as_str(),
                    })
                }),
            None => None,
        };
        let mut value = node_json(node, this_node, statuses.get(&node.node_id), &runtime);
        value["owner"] = owner.unwrap_or(serde_json::Value::Null);
        out.push(value);
    }
    Ok(out)
}

/// What `caller` may do on the nodes it sees.
fn node_capabilities(caller: NodeAdminCaller) -> serde_json::Value {
    let operator = caller == NodeAdminCaller::Operator;
    json!({
        "scope": if operator { "all" } else { "owned" },
        "can_edit_runtime": true,
        "can_edit_labels": operator,
        "can_assign_owner": operator,
    })
}

/// `GET /api/config/nodes`: every node for an operator; the caller's own
/// nodes for a node owner.
#[utoipa::path(
    get,
    path = "/api/config/nodes",
    responses(
        (status = 200, description = "The nodes the caller administers.", body = serde_json::Value),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The caller may not do this."),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
    ),
)]
pub async fn get_nodes(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<NodeAdminCaller>,
) -> Result<Json<serde_json::Value>, Response> {
    let cfg = state.config.read().await.clone();
    let this_node = this_node_id(&cfg)?;
    let library = state.library_snapshot().await;
    let nodes = visible_nodes(&library, caller).await?;
    let nodes = nodes_json(&library, &this_node, &nodes).await?;
    Ok(Json(json!({
        "node_id": this_node,
        "nodes": nodes,
        "capabilities": node_capabilities(caller),
    })))
}

/// `GET /api/cluster/status`: cluster-wide health every node administrator
/// may see (node counts, committed capacity, active jobs, delivery backlog),
/// plus detail for the nodes the caller may see.
#[utoipa::path(
    get,
    path = "/api/cluster/status",
    responses(
        (status = 200, description = "Cluster-wide node health.", body = serde_json::Value),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The caller may not do this."),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
    ),
)]
pub async fn get_cluster_status(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<NodeAdminCaller>,
) -> Result<Json<serde_json::Value>, Response> {
    let cfg = state.config.read().await.clone();
    let this_node = this_node_id(&cfg)?;
    let library = state.library_snapshot().await;
    let all = bookclerk_library::control_plane::list_nodes(&library)
        .await
        .map_err(events_error)?;
    let statuses = node_statuses(&library).await?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut live = 0u64;
    let mut drained = 0u64;
    let mut host_nodes = 0u64;
    let mut cpu_budget = 0i64;
    let mut cpu_reserved = 0i64;
    let mut memory_budget = 0i64;
    let mut memory_reserved = 0i64;
    let mut running_guests = 0i64;
    for node in &all {
        if node.kind == bookclerk_library::control_plane::NodeKind::Host {
            host_nodes += 1;
        }
        let Some(status) = statuses.get(&node.node_id) else {
            continue;
        };
        if !status.is_live(now_ms) {
            continue;
        }
        live += 1;
        if !status.schedulable {
            drained += 1;
        }
        cpu_budget += status.cpu_budget_millicores;
        cpu_reserved += status.reserved.cpu_millicores;
        memory_budget += status.memory_budget_bytes.unwrap_or(0);
        memory_reserved += status.reserved.memory_bytes;
        running_guests += status.running_guests;
    }
    let active_jobs = library.count_active_jobs().await.map_err(events_error)?;
    let events = library
        .event_delivery_metrics()
        .await
        .map_err(events_error)?;
    let visible = visible_nodes(&library, caller).await?;
    let nodes = nodes_json(&library, &this_node, &visible).await?;
    let leases: Vec<serde_json::Value> = bookclerk_library::control_plane::list_leases(&library)
        .await
        .map_err(events_error)?
        .into_iter()
        .map(|lease| {
            json!({
                "name": lease.name,
                "holder": lease.holder,
                "token": lease.token,
                "held": lease.is_held_at(now_ms),
                "expires_at_ms": lease.expires_at_ms,
            })
        })
        .collect();
    // Whether the database is exactly the schema this binary applies; the
    // migration lease (above) names the last migrator.
    let schema = match bookclerk_library::current_schema_state(library.db()).await {
        Ok(state) => json!({
            "state": state.to_string(),
            "compatible": bookclerk_library::schema_state_compatible(&state),
        }),
        Err(err) => json!({ "state": null, "compatible": false, "error": err.to_string() }),
    };
    Ok(Json(json!({
        "cluster": {
            "nodes": all.len(),
            "live": live,
            "drained": drained,
            "host_nodes": host_nodes,
            "cpu_budget_millicores": cpu_budget,
            "cpu_reserved_millicores": cpu_reserved,
            "memory_budget_bytes": memory_budget,
            "memory_reserved_bytes": memory_reserved,
            "running_guests": running_guests,
            "active_jobs": active_jobs,
            "deliveries": {
                "pending": events.pending,
                "running": events.running,
                "dead_letter": events.dead_letter,
                "oldest_pending_age_secs": events.oldest_pending_age_secs,
            },
            "leases": leases,
            "schema": schema,
        },
        "node_id": this_node,
        "nodes": nodes,
        "capabilities": node_capabilities(caller),
    })))
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
        "kind": node.kind.as_str(),
        "owner_user_id": node.owner_user_id,
        // A branch's two sharing settings, as it last reported them.
        "share_books": node.share_books,
        "include_branches": node.include_branches,
    })
}

/// The clerk actor for a node-administration `caller`: the library's
/// clerk rules (an Owner reaches every owner node) are checked on it.
fn clerk_actor(caller: NodeAdminCaller) -> bookclerk_library::control_plane::EnrollmentActor {
    match caller {
        NodeAdminCaller::Operator => bookclerk_library::control_plane::EnrollmentActor::Operator {
            id: "operator".into(),
        },
        NodeAdminCaller::NodeOwner { user_id } => {
            bookclerk_library::control_plane::EnrollmentActor::User { user_id }
        }
    }
}

/// `GET /api/config/clerks`: the owner nodes (this one and its clerks) for
/// Settings → Clerks, shaped like `GET /api/config/nodes`. The operator or
/// an Owner sees every one, as `DELETE /api/config/nodes/{node_id}` lets
/// them remove any; another user only the ones they own, which the nodes
/// listing already shows them. Branches are not listed.
#[utoipa::path(
    get,
    path = "/api/config/clerks",
    responses(
        (status = 200, description = "This node and its clerks.", body = serde_json::Value),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The caller may not do this."),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
    ),
)]
pub async fn get_clerks(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<NodeAdminCaller>,
) -> Result<Json<serde_json::Value>, Response> {
    let cfg = state.config.read().await.clone();
    let this_node = this_node_id(&cfg)?;
    let library = state.library_snapshot().await;
    let nodes =
        bookclerk_library::control_plane::owner_nodes_visible_to(&library, &clerk_actor(caller))
            .await
            .map_err(events_error)?;
    let nodes = nodes_json(&library, &this_node, &nodes).await?;
    Ok(Json(json!({ "node_id": this_node, "nodes": nodes })))
}

/// `DELETE /api/config/nodes/{node_id}`: remove an owner node (a clerk, or
/// any node sharing this database) that is no longer running from the
/// library's node list. The operator or an Owner may remove any; another
/// user only one they own. A running node, this node, or a branch is
/// refused (a branch is revoked instead).
#[utoipa::path(
    delete,
    path = "/api/config/nodes/{node_id}",
    params(("node_id" = String, Path, description = "Node id.")),
    responses(
        (status = 200, description = "The owner node was removed.", body = serde_json::Value),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The caller may not do this."),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
        (status = 404, description = "No such node, or not one the caller administers."),
    ),
)]
pub async fn delete_owner_node(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<NodeAdminCaller>,
    axum::extract::Path(node_id): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, Response> {
    let cfg = state.config.read().await.clone();
    let this_node = this_node_id(&cfg)?;
    let library = state.library_snapshot().await;
    let actor = clerk_actor(caller);
    let node_id = node_id.trim().to_string();
    bookclerk_library::control_plane::remove_owner_node(&library, &actor, &node_id, &this_node)
        .await
        .map_err(events_error)?;
    Ok(Json(json!({ "node_id": node_id, "removed": true })))
}

/// `PUT /api/config/nodes/{node_id}/runtime` body: any subset of the
/// node's runtime document.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
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
/// or drain switch. Applies at that node's next heartbeat. A node owner may
/// edit only the nodes it owns.
#[utoipa::path(
    put,
    path = "/api/config/nodes/{node_id}/runtime",
    params(("node_id" = String, Path, description = "Node id.")),
    request_body = PutNodeRuntime,
    responses(
        (status = 200, description = "The node's runtime settings after the change.", body = serde_json::Value),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The caller may not do this."),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
        (status = 404, description = "No such node, or not one the caller administers."),
    ),
)]
pub async fn put_node_runtime(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<NodeAdminCaller>,
    axum::extract::Path(node_id): axum::extract::Path<String>,
    Json(body): Json<PutNodeRuntime>,
) -> Result<Json<serde_json::Value>, Response> {
    let library = state.library_snapshot().await;
    let node_id = node_id.trim().to_string();
    let actor = match caller {
        NodeAdminCaller::Operator => {
            bookclerk_library::control_plane::load_node(&library, &node_id)
                .await
                .map_err(events_error)?;
            operator_actor()
        }
        NodeAdminCaller::NodeOwner { user_id } => {
            bookclerk_library::control_plane::verify_node_owner(&library, user_id, &node_id)
                .await
                .map_err(events_error)?
        }
    };
    let document = bookclerk_library::control_plane::update_node_runtime(
        &library,
        &actor,
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
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(crate) struct PutNodeLabels {
    /// The node's complete operator label set.
    labels: Vec<String>,
}

/// `PUT /api/config/nodes/{node_id}/labels`: replace a node's placement
/// labels. The scheduler re-plans on the wakeup this publishes.
#[utoipa::path(
    put,
    path = "/api/config/nodes/{node_id}/labels",
    params(("node_id" = String, Path, description = "Node id.")),
    request_body = PutNodeLabels,
    responses(
        (status = 200, description = "The node's labels after the change.", body = serde_json::Value),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
        (status = 404, description = "No such node, or not one the caller administers."),
    ),
)]
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

/// `PUT /api/config/nodes/{node_id}/owner` body.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(crate) struct PutNodeOwner {
    /// User who owns the node, or `null` to clear.
    user_id: Option<i64>,
}

/// `PUT /api/config/nodes/{node_id}/owner`: assign or clear a node's owner.
/// The owner may then edit that node's runtime settings without operator
/// authority.
#[utoipa::path(
    put,
    path = "/api/config/nodes/{node_id}/owner",
    params(("node_id" = String, Path, description = "Node id.")),
    request_body = PutNodeOwner,
    responses(
        (status = 200, description = "The node's owner after the change.", body = serde_json::Value),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
        (status = 404, description = "No such node, or not one the caller administers."),
    ),
)]
pub async fn put_node_owner(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(node_id): axum::extract::Path<String>,
    Json(body): Json<PutNodeOwner>,
) -> Result<Json<serde_json::Value>, Response> {
    let library = state.library_snapshot().await;
    let node = bookclerk_library::control_plane::set_node_owner(
        &library,
        &operator_actor(),
        node_id.trim(),
        body.user_id,
    )
    .await
    .map_err(events_error)?;
    Ok(Json(json!({
        "node_id": node.node_id,
        "owner_user_id": node.owner_user_id,
    })))
}

/// `PUT /api/config/plugin-instances/{id}/placement` body.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(crate) struct PutPluginPlacement {
    /// Nodes that should run the instance: a number (0 stops every
    /// replica) or `"all"`.
    #[schema(schema_with = crate::openapi::replicas_schema)]
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
#[utoipa::path(
    put,
    path = "/api/config/plugin-instances/{id}/placement",
    params(("id" = String, Path, description = "Plugin instance id.")),
    request_body = PutPluginPlacement,
    responses(
        (status = 200, description = "The scheduler now places the instance.", body = serde_json::Value),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
        (status = 404, description = "No such instance."),
    ),
)]
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
/// instance to hand-managed deployments; its deployments stay as they
/// are.
#[utoipa::path(
    delete,
    path = "/api/config/plugin-instances/{id}/placement",
    params(("id" = String, Path, description = "Plugin instance id.")),
    responses(
        (status = 200, description = "The instance is back to manual deployment.", body = serde_json::Value),
        (status = 400, description = "The body is invalid."),
        (status = 500, description = "The configuration could not be read or written."),
        (status = 404, description = "No such instance."),
    ),
)]
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

/// 409 body carrying the revision that won.
fn conflict(current_revision: i64) -> Response {
    (
        StatusCode::CONFLICT,
        Json(RevisionConflict {
            error: "revision_conflict",
            current_revision,
        }),
    )
        .into_response()
}

/// HTTP status for a library error, by its variant.
pub(crate) fn library_status(err: &bookclerk_library::LibraryError) -> StatusCode {
    use bookclerk_library::LibraryError;
    match err {
        LibraryError::Conflict(_) => StatusCode::CONFLICT,
        LibraryError::Forbidden(_) => StatusCode::FORBIDDEN,
        LibraryError::Invalid(_) | LibraryError::InvalidEmail => StatusCode::BAD_REQUEST,
        LibraryError::NotFound(_) => StatusCode::NOT_FOUND,
        LibraryError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Maps a library configuration error onto an HTTP status.
pub(crate) fn events_error(err: bookclerk_library::LibraryError) -> Response {
    (
        library_status(&err),
        Json(json!({
            "error": err.to_string(),
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
        node_api_tls_bound: std::sync::OnceLock::new(),
        library_swaps: tokio::sync::watch::Sender::new(0),
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
        let session = bootstrap_control_plane(&store, files.path(), &scratch, None, &config)
            .await
            .expect("bootstrap");
        overlay_events(&mut config, &session.events, &session.cluster_id);
        (store, session, config, files)
    }

    #[tokio::test]
    async fn domain_routes_return_typed_bodies_and_revision_conflicts() {
        use super::{
            get_events_domain, get_output_domain, put_events_domain, put_output_domain,
            DomainReplayed, EventsDomainChange, OutputDomainChange, PutEventsDomainRequest,
            PutOutputDomainRequest,
        };
        use axum::extract::State;
        use axum::http::StatusCode;
        use axum::Json;
        use http_body_util::BodyExt;

        let _dek = crate::auth::tests::process_dek_lock().await;
        let (store, _session, config, _files) = enrolled().await;
        let state = std::sync::Arc::new(control_plane_test_state(store, config));

        // GET: the revision metadata and the settings share one object.
        let Json(current) = get_events_domain(State(state.clone())).await.unwrap();
        let current = serde_json::to_value(&current).unwrap();
        assert_eq!(current["namespace"], "core.events");
        assert_eq!(current["scope_type"], "cluster");
        assert_eq!(current["revision"], 1);
        assert!(current["retention_days"].is_u64());
        assert!(current["dead_letter_retention_days"].is_u64());
        assert!(current["concurrency"].is_u64());

        // PUT against the current revision applies and reports the next one.
        let Json(applied) = put_events_domain(
            State(state.clone()),
            Json(PutEventsDomainRequest {
                expected_revision: 1,
                retention_days: 9,
                dead_letter_retention_days: 45,
                concurrency: 2,
            }),
        )
        .await
        .unwrap();
        let applied = serde_json::to_value(&applied).unwrap();
        assert_eq!(applied["namespace"], "core.events");
        assert_eq!(applied["revision"], 2);
        assert_eq!(applied["retention_days"], 9);
        assert_eq!(applied["concurrency"], 2);
        assert!(applied.get("replayed").is_none());

        // PUT against a stale revision is a 409 naming the revision that won.
        let conflict = put_events_domain(
            State(state.clone()),
            Json(PutEventsDomainRequest {
                expected_revision: 1,
                retention_days: 3,
                dead_letter_retention_days: 3,
                concurrency: 1,
            }),
        )
        .await
        .expect_err("stale revision must be refused");
        assert_eq!(conflict.status(), StatusCode::CONFLICT);
        let body = conflict.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({ "error": "revision_conflict", "current_revision": 2 })
        );

        // A replayed write renders the namespace, the flag and the revision.
        let replayed = serde_json::to_value(EventsDomainChange::Replayed(DomainReplayed::new(
            "core.events",
            2,
        )))
        .unwrap();
        assert_eq!(
            replayed,
            serde_json::json!({ "namespace": "core.events", "replayed": true, "revision": 2 })
        );

        // The output domain follows the same shape, with the settings under
        // `document`.
        let Json(output) = get_output_domain(State(state.clone())).await.unwrap();
        let output_value = serde_json::to_value(&output).unwrap();
        assert_eq!(output_value["namespace"], "core.output");
        assert_eq!(output_value["revision"], 1);
        assert!(output_value["document"].is_object());
        let Json(applied) = put_output_domain(
            State(state.clone()),
            Json(PutOutputDomainRequest {
                expected_revision: 1,
                document: output.document.clone(),
            }),
        )
        .await
        .unwrap();
        let applied = serde_json::to_value(&applied).unwrap();
        assert_eq!(applied["namespace"], "core.output");
        assert_eq!(applied["revision"], 2);
        assert_eq!(applied["document"], output_value["document"]);
        let conflict = put_output_domain(
            State(state.clone()),
            Json(PutOutputDomainRequest {
                expected_revision: 1,
                document: output.document.clone(),
            }),
        )
        .await
        .expect_err("stale revision must be refused");
        assert_eq!(conflict.status(), StatusCode::CONFLICT);
        let replayed = serde_json::to_value(OutputDomainChange::Replayed(DomainReplayed::new(
            bookclerk_library::control_plane::OUTPUT_NAMESPACE,
            2,
        )))
        .unwrap();
        assert_eq!(replayed["namespace"], "core.output");
        assert_eq!(replayed["replayed"], true);
    }

    #[tokio::test]
    async fn stale_read_cannot_replace_a_newer_publication() {
        let _dek = crate::auth::tests::process_dek_lock().await;
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
        let _dek = crate::auth::tests::process_dek_lock().await;
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
        let _dek = crate::auth::tests::process_dek_lock().await;
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

    #[tokio::test]
    async fn output_publication_is_monotonic_and_reload_keeps_host_scratch() {
        use super::{
            finish_reload_output, load_output_publication, publish_loaded_output, reconcile_output,
            LoadedOutput,
        };
        use bookclerk_library::control_plane::{replace_output, OutputReplace};

        let _dek = crate::auth::tests::process_dek_lock().await;
        let (store, session, mut config, _files) = enrolled().await;
        bookclerk_library::control_plane::overlay_core_documents(&mut config, &session);
        let state = control_plane_test_state(store.clone(), config);
        let stale = load_output_publication(&store).await.unwrap();
        assert_eq!(stale.document.revision, 1);

        let mut body = stale.document.body.clone();
        body.set("output.naming_profile", "classic").unwrap();
        let applied = replace_output(&store, &operator(), 1, &body, "output-pub-2")
            .await
            .unwrap();
        let OutputReplace::Applied(doc) = applied else {
            panic!("{applied:?}");
        };
        // Another host's write reaches this daemon through the reconciler.
        reconcile_output(&state).await.unwrap();
        assert!(
            !publish_loaded_output(&state, &stale, EventsPublish::Monotonic).await,
            "an older read must not publish"
        );
        let live = state.config.read().await.clone();
        assert_eq!(
            live.output.naming_profile,
            bookclerk_config::NamingProfile::Classic
        );
        assert_eq!(live.output_revision, Some(doc.revision));

        // Reload: the candidate's TOML values lose to the live revision, but
        // its host-local scratch directory is kept.
        let mut candidate = Config::default();
        candidate.output.in_progress = Some("/reloaded/scratch".into());
        candidate.output.naming_profile = bookclerk_config::NamingProfile::Audiobookshelf;
        finish_reload_output(&live, &mut candidate, &stale, false);
        assert_eq!(
            candidate.output.naming_profile,
            bookclerk_config::NamingProfile::Classic
        );
        assert_eq!(candidate.output_revision, live.output_revision);
        assert_eq!(
            candidate.output.in_progress.as_deref(),
            Some(std::path::Path::new("/reloaded/scratch"))
        );

        // A database swap installs the other cluster's document even at a
        // lower revision.
        let swapped = LoadedOutput {
            cluster_id: "swapped-cluster".into(),
            document: stale.document.clone(),
        };
        let from = candidate.clone();
        finish_reload_output(&from, &mut candidate, &swapped, true);
        assert_eq!(
            candidate.output.naming_profile,
            bookclerk_config::NamingProfile::Audiobookshelf
        );
        assert_eq!(candidate.output_revision, Some(1));
        assert_eq!(
            candidate.output_authority.as_deref(),
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
    async fn daemon_reconcile_installs_and_runs_a_local_archive() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        use std::collections::BTreeMap;
        use std::path::PathBuf;

        use bookclerk_config::Isolation;
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
        let session = bootstrap_control_plane(
            &store,
            files_path,
            &scratch,
            None,
            &bookclerk_config::Config::default(),
        )
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
            bookclerk_library::control_plane::load_active_version(&store, &instance.plugin_key)
                .await
                .unwrap()
                .expect("active release");
        let artifact = bookclerk_plugin_host::host_artifact(&release).unwrap();
        assert!(
            artifact.url.starts_with("file://"),
            "a guest binary over the blob cap is referenced in place: {}",
            artifact.url
        );
        assert!(bookclerk_plugin_host::version_cache_path(
            files_path,
            &artifact.payload_root_sha256
        )
        .unwrap()
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
        let _dek = crate::auth::tests::process_dek_lock().await;
        use std::collections::BTreeMap;
        use std::path::PathBuf;

        use bookclerk_config::Isolation;
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
        let session = bootstrap_control_plane(
            &store,
            files_path,
            &scratch,
            None,
            &bookclerk_config::Config::default(),
        )
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
        let _dek = crate::auth::tests::process_dek_lock().await;
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
            &bookclerk_config::Config::default(),
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
