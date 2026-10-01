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
    EventsSettingsV1, CONFIG_RECONCILE_INTERVAL,
};
use bookclerk_library::LibraryStore;
use serde::Deserialize;
use serde_json::json;

use crate::api::AppState;

/// Operator actor recorded on configuration commits from the daemon.
fn operator_actor() -> ConfigActor {
    ConfigActor::Operator {
        id: "operator".into(),
    }
}

/// Re-reads `core.events` until the process stops.
pub fn spawn_config_reconciler(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(CONFIG_RECONCILE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(err) = reconcile_events(&state).await {
                tracing::warn!(error = %err, "configuration reconcile failed");
            }
        }
    });
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

/// Re-reads local plugin deployments until the process stops.
pub fn spawn_deployment_reconciler(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(CONFIG_RECONCILE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(err) = reconcile_deployments(&state).await {
                tracing::warn!(error = %err, "plugin deployment reconcile failed");
            }
        }
    });
}

/// Reconciles present deployments for this process's host.
async fn reconcile_deployments(state: &AppState) -> anyhow::Result<()> {
    let config = state.config.read().await.clone();
    let store = state.library.read().await.clone();
    let host =
        bookclerk_library::control_plane::load_or_create_host_identity(&config.paths().files_dir)?;
    let runtime = bookclerk_plugin_host::LiveDeploymentRuntime {
        config: Arc::clone(&state.config),
        store: Arc::clone(&state.library),
        sources: Arc::clone(&state.sources),
        integrations: Arc::clone(&state.integrations),
        destinations: Arc::clone(&state.destinations),
    };
    bookclerk_plugin_host::reconcile_local_deployments(
        &store,
        &config,
        &host.host_id,
        &std::collections::HashMap::new(),
        &runtime,
    )
    .await?;
    Ok(())
}

/// `PUT /api/config/plugin-instances/{id}/config` body.
#[derive(Debug, Deserialize)]
pub(crate) struct PutPluginInstanceConfig {
    /// Revision the caller last observed.
    expected_revision: i64,
    /// Scalar settings. Secret values are not accepted here.
    #[serde(default)]
    settings: std::collections::BTreeMap<String, bookclerk_library::control_plane::SettingValue>,
    /// Secret ref names. Ciphertext stays in `encrypted_secrets`.
    #[serde(default)]
    secret_refs: Vec<bookclerk_library::control_plane::InstanceSecretRefV1>,
}

/// `PUT /api/config/plugin-instances/{id}/config`.
///
/// Operator principal only. The route is mounted on the operator router.
pub async fn put_plugin_instance_config(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<PutPluginInstanceConfig>,
) -> Result<Json<serde_json::Value>, Response> {
    use bookclerk_library::control_plane::{
        load_plugin_instance, replace_instance_config, InstanceConfigReplace,
        InstancePackagePolicy, PluginInstanceConfigV1, PluginInstanceId,
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
    let cfg = state.config.read().await.clone();
    let policy = match bookclerk_plugin_host::graphicaudio_plugin_key(&cfg)
        .map_err(|err| events_error(bookclerk_library::LibraryError::Other(anyhow::anyhow!(err))))?
    {
        Some(key) if key == instance.plugin_key => InstancePackagePolicy::GraphicAudio,
        _ => InstancePackagePolicy::Generic,
    };
    let document = PluginInstanceConfigV1 {
        settings: body.settings,
        secret_refs: body.secret_refs,
    };
    let outcome = replace_instance_config(
        &library,
        &operator_actor(),
        &instance_id,
        policy,
        body.expected_revision,
        &document,
        &uuid::Uuid::new_v4().to_string(),
    )
    .await
    .map_err(events_error)?;
    match outcome {
        InstanceConfigReplace::Applied(doc) => Ok(Json(json!({
            "plugin_instance_id": instance_id.to_string(),
            "revision": doc.revision,
            "settings": doc.body.settings,
            "secret_refs": doc.body.secret_refs,
        }))),
        InstanceConfigReplace::Replayed { revision } => Ok(Json(json!({
            "plugin_instance_id": instance_id.to_string(),
            "replayed": true,
            "revision": revision,
        }))),
        InstanceConfigReplace::Conflict { current_revision } => Err(conflict(current_revision)),
    }
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
    let status = if message.contains("unauthorized configuration write") {
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
        event_node_id: OnceLock::new(),
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
        let session = bootstrap_control_plane(&store, files.path(), None, &config.events)
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
}
