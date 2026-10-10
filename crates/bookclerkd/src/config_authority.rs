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
    let runtime = state.deployment_runtime();
    let packages = bookclerk_plugin_host::load_authorized_local_packages(&config.paths().files_dir)
        .map_err(|err| anyhow::anyhow!(err))?;
    bookclerk_plugin_host::reconcile_local_deployments(
        &store,
        &config,
        &host.host_id,
        &packages,
        runtime.as_ref(),
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

    fn file_tree_contains(root: &std::path::Path, needle: &[u8]) -> bool {
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if std::fs::read(&path).ok().as_deref() == Some(needle) {
                    return true;
                }
            }
        }
        false
    }

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
    async fn daemon_reconcile_installs_a_local_archive_to_healthy() {
        use std::collections::BTreeMap;
        use std::path::PathBuf;

        use bookclerk_config::{EventsConfig, Isolation};
        use bookclerk_library::control_plane::{
            bootstrap_control_plane, create_plugin_instance, ensure_plugin_deployment,
            import_instance_config_if_absent, load_deployment, load_observation, ConfigActor,
            DeploymentStatus, InstancePackagePolicy, PluginInstanceConfigV1, SettingValue,
            DESIRED_PRESENT,
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
        let session = bootstrap_control_plane(&store, files_path, None, &EventsConfig::default())
            .await
            .expect("bootstrap");
        let actor = ConfigActor::Bootstrap;
        let instance = create_plugin_instance(&store, &actor, &plugin_key)
            .await
            .expect("instance");
        let mut settings = BTreeMap::new();
        settings.insert("access".into(), SettingValue::String("device".into()));
        import_instance_config_if_absent(
            &store,
            &actor,
            &instance.id,
            InstancePackagePolicy::GraphicAudio,
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
        let deployment =
            ensure_plugin_deployment(&store, &actor, &instance.id, &session.host.host_id)
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

        let obs = load_observation(&store, &deployment.deployment_id, &session.host.host_id)
            .await
            .unwrap()
            .expect("observation");
        assert_eq!(obs.status, DeploymentStatus::Healthy, "{}", obs.detail);
        let desired = load_deployment(&store, &deployment.deployment_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(desired.desired, DESIRED_PRESENT);
        let mut committed = false;
        for entry in std::fs::read_dir(files_path.join("plugins")).unwrap() {
            let path = entry.unwrap().path();
            if path.join("receipt.json").is_file() && path.join("plugin.toml").is_file() {
                committed = true;
            }
        }
        assert!(committed, "install did not commit a receipt");

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
        let job_registry = crate::registry::registry_for_job(&state)
            .await
            .expect("job registry");
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
            InstancePackagePolicy, PluginInstanceConfigV1, SettingValue,
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
        let session = bootstrap_control_plane(&store, files_path, None, &EventsConfig::default())
            .await
            .expect("bootstrap");
        let actor = ConfigActor::Bootstrap;
        let first = create_plugin_instance(&store, &actor, &plugin_key)
            .await
            .expect("first instance");
        let second = create_plugin_instance(&store, &actor, &plugin_key)
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
                InstancePackagePolicy::GraphicAudio,
                &PluginInstanceConfigV1 {
                    settings,
                    secret_refs: Vec::new(),
                },
                operation,
            )
            .await
            .expect("import");
            let deployment =
                ensure_plugin_deployment(&store, &actor, &instance.id, &session.host.host_id)
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
            let obs = load_observation(&store, deployment_id, &session.host.host_id)
                .await
                .unwrap()
                .expect("observation");
            assert_eq!(obs.status, DeploymentStatus::Healthy, "{}", obs.detail);
        }

        let runtime = state.deployment_runtime();
        let registry = crate::registry::registry_for_job(&state)
            .await
            .expect("job registry");
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
        assert!(
            registry.get(&plugin_key).is_none(),
            "plugin key must not pick one of two instances"
        );
        assert!(registry.get("graphicaudio").is_none());
        let ambiguous = match registry.require(&plugin_key) {
            Err(err) => err.to_string(),
            Ok(_) => panic!("plugin key must be ambiguous"),
        };
        assert!(ambiguous.contains("plugin instance id"), "{ambiguous}");
        {
            let mut cfg = state.config.write().await;
            cfg.output.local.enabled = true;
        }
        store
            .upsert_account("acct", "us", None, true, "graphicaudio")
            .await
            .expect("account");
        let mut book = bookclerk_library::NewBook::minimal("B0PHASE2", "acct", "us", "Phase Two");
        book.source = "graphicaudio".into();
        store.upsert_book(&book).await.expect("book");
        let acquire_err = crate::jobs::run_acquire(&state, Some("B0PHASE2"), None, None)
            .await
            .expect_err("ambiguous acquire");
        assert!(
            acquire_err.to_string().contains("plugin instance id"),
            "{acquire_err}"
        );

        for instance in [&first, &second] {
            if let Some(pid) = runtime.tracked_guest_pid(instance.id.as_str()) {
                kill_pid(pid);
            }
        }
    }

    struct CountingIntegration {
        id: &'static str,
        key: String,
        instance: Option<String>,
        stops: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl bookclerk_integrations::Integration for CountingIntegration {
        fn id(&self) -> &str {
            self.id
        }

        fn plugin_key(&self) -> &str {
            &self.key
        }

        fn plugin_instance_id(&self) -> Option<&str> {
            self.instance.as_deref()
        }

        async fn start(
            &self,
            _ctx: bookclerk_integrations::IntegrationContext,
        ) -> bookclerk_integrations::Result<()> {
            Ok(())
        }

        async fn stop(&self) -> bookclerk_integrations::Result<()> {
            self.stops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        async fn health(
            &self,
        ) -> bookclerk_integrations::Result<bookclerk_integrations::IntegrationHealth> {
            Ok(bookclerk_integrations::IntegrationHealth {
                id: self.id.to_string(),
                enabled: true,
                ok: true,
                detail: None,
            })
        }

        async fn deliver_domain_event(
            &self,
            _event: bookclerk_integrations::DomainEvent,
        ) -> bookclerk_integrations::Result<bookclerk_integrations::EventResult> {
            Ok(bookclerk_integrations::EventResult::Ack)
        }
    }

    /// Source registered under the plugin key, with no deployment instance id.
    struct KeyOnlySource {
        key: String,
    }

    #[async_trait::async_trait]
    impl bookclerk_source::ContentSource for KeyOnlySource {
        fn id(&self) -> &str {
            "transitional-source"
        }

        fn plugin_key(&self) -> &str {
            &self.key
        }

        fn portal_auth_mode(&self) -> bookclerk_source::PortalAuthMode {
            bookclerk_source::PortalAuthMode::Password
        }

        fn portal_brand(&self) -> bookclerk_source::SourceBrand {
            bookclerk_source::SourceBrand {
                id: "transitional-source",
                name: "Transitional",
                bg: "#000000",
                fg: "#ffffff",
                accent: "#111111",
                icon_url: "https://example.invalid/icon",
            }
        }

        async fn login(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _opts: bookclerk_source::LoginOptions,
        ) -> bookclerk_source::Result<bookclerk_source::SourceAccount> {
            Err(bookclerk_source::SourceError::api("transitional stub"))
        }

        async fn list_accounts(
            &self,
            _scope: &bookclerk_library::SourceScope,
        ) -> bookclerk_source::Result<Vec<bookclerk_source::SourceAccount>> {
            Err(bookclerk_source::SourceError::api("transitional stub"))
        }

        async fn scan(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _opts: bookclerk_source::ScanOptions,
        ) -> bookclerk_source::Result<bookclerk_source::ScanSummary> {
            Err(bookclerk_source::SourceError::api("transitional stub"))
        }

        async fn fetch_title(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _account_id: &str,
            _title_id: &str,
            _opts: &bookclerk_source::FetchOptions,
        ) -> bookclerk_source::Result<bookclerk_source::SourceFetch> {
            Err(bookclerk_source::SourceError::api("transitional stub"))
        }
    }

    fn install_staged_archive(config: &Config, files_path: &std::path::Path, package_key: &str) {
        let packages =
            bookclerk_plugin_host::load_authorized_local_packages(files_path).expect("packages");
        let package = packages
            .get(package_key)
            .unwrap_or_else(|| panic!("staged package {package_key}"));
        let plugins_root = files_path.join("plugins");
        std::fs::create_dir_all(&plugins_root).unwrap();
        let lock = bookclerk_plugin_catalog::PluginMutationLock::acquire(files_path).expect("lock");
        let opts = bookclerk_plugin_catalog::InstallOptions {
            plugins_root,
            offline: true,
            trust: bookclerk_plugin_catalog::TrustPolicy::allow_unverified_publisher(),
            skip_health: true,
            ..bookclerk_plugin_catalog::InstallOptions::default()
        };
        let outcome = bookclerk_plugin_host::install_local_archive_with_configured_aliases(
            config,
            &lock,
            &package.archive,
            &package.manifest,
            &opts,
        )
        .expect("install transitional guest");
        bookclerk_plugin_catalog::Installer::commit(&outcome).expect("commit install");
    }

    #[tokio::test]
    async fn daemon_reload_keeps_deployed_sessions() {
        use std::path::PathBuf;
        use std::sync::atomic::Ordering;
        use std::sync::Arc;

        use bookclerk_config::Isolation;
        use bookclerk_library::control_plane::{
            bootstrap_control_plane, create_plugin_instance, ensure_plugin_deployment,
            import_instance_config_if_absent, load_observation, ConfigActor, DeploymentStatus,
            InstancePackagePolicy, PluginInstanceConfigV1,
        };

        let files = tempfile::tempdir().expect("files");
        let files_path = files.path();
        std::fs::write(
            files_path.join("config.toml"),
            "[plugins]\nisolation = \"off\"\n\n[daemon.auth]\nenabled = false\n",
        )
        .unwrap();
        let source_toml = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../bookclerk-plugins/optional/source-graphicaudio/plugin.toml");
        let local_toml_src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../bookclerk-plugins/platform/destination-local/plugin.toml");
        // Job triggers make this guest account-bearing. The deployment loop
        // spawns the operator account, so the fixture keeps the storage
        // entrypoint and drops `[triggers]`.
        let local_toml = files_path.join("local-plugin.toml");
        let local_manifest = std::fs::read_to_string(&local_toml_src).unwrap();
        let local_manifest = local_manifest
            .split("[triggers]")
            .next()
            .unwrap_or(&local_manifest);
        std::fs::write(&local_toml, local_manifest).unwrap();
        let source_key = stage_graphicaudio_package(files_path, &source_toml);
        let local_key = stage_plugin_package(
            files_path,
            "local",
            &local_toml,
            "bookclerk-plugin-destination-local",
            "output",
            "local",
            "deny",
        );

        let db = bookclerk_plugin_database_sqlite::open(&files_path.join("library.db"))
            .await
            .expect("sqlite");
        bookclerk_library::apply_host_schema(&db)
            .await
            .expect("schema");
        let store = bookclerk_library::LibraryStore::from_connection(db);
        let session = bootstrap_control_plane(
            &store,
            files_path,
            None,
            &bookclerk_config::EventsConfig::default(),
        )
        .await
        .expect("bootstrap");
        let actor = ConfigActor::Bootstrap;
        let source_instance = create_plugin_instance(&store, &actor, &source_key)
            .await
            .expect("source instance");
        let mut source_settings = std::collections::BTreeMap::new();
        source_settings.insert(
            "access".into(),
            bookclerk_library::control_plane::SettingValue::String("device".into()),
        );
        import_instance_config_if_absent(
            &store,
            &actor,
            &source_instance.id,
            InstancePackagePolicy::GraphicAudio,
            &PluginInstanceConfigV1 {
                settings: source_settings,
                secret_refs: Vec::new(),
            },
            "import-reload-ga",
        )
        .await
        .expect("import source");
        let local_instance = create_plugin_instance(&store, &actor, &local_key)
            .await
            .expect("local instance");
        let local_root = files_path.join("instance-audiobooks");
        std::fs::create_dir_all(&local_root).unwrap();
        let mut local_settings = std::collections::BTreeMap::new();
        local_settings.insert(
            "prefix".into(),
            bookclerk_library::control_plane::SettingValue::String("library".into()),
        );
        local_settings.insert(
            "root".into(),
            bookclerk_library::control_plane::SettingValue::String(
                local_root.display().to_string(),
            ),
        );
        import_instance_config_if_absent(
            &store,
            &actor,
            &local_instance.id,
            InstancePackagePolicy::Generic,
            &PluginInstanceConfigV1 {
                settings: local_settings,
                secret_refs: Vec::new(),
            },
            "import-reload-local",
        )
        .await
        .expect("import local");
        let mut grants = bookclerk_plugin_host::PluginGrantStore::default();
        for (toml, key) in [(&source_toml, &source_key), (&local_toml, &local_key)] {
            let manifest = bookclerk_plugin_host::PluginManifest::parse(
                &std::fs::read_to_string(toml).unwrap(),
            )
            .unwrap();
            let mut grant = bookclerk_plugin_host::consent_request_alias(&manifest);
            grant.plugin_key = key.clone();
            grants.upsert(grant);
        }
        grants.save(files_path).unwrap();
        let source_deployment =
            ensure_plugin_deployment(&store, &actor, &source_instance.id, &session.host.host_id)
                .await
                .expect("source deployment");
        let local_deployment =
            ensure_plugin_deployment(&store, &actor, &local_instance.id, &session.host.host_id)
                .await
                .expect("local deployment");

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
        for deployment_id in [
            &source_deployment.deployment_id,
            &local_deployment.deployment_id,
        ] {
            let obs = load_observation(&store, deployment_id, &session.host.host_id)
                .await
                .unwrap()
                .expect("observation");
            assert_eq!(obs.status, DeploymentStatus::Healthy, "{}", obs.detail);
        }

        let runtime = state.deployment_runtime();
        let source_id = source_instance.id.as_str();
        let source_pid = runtime.tracked_guest_pid(source_id).expect("source pid");
        let local_pid = runtime
            .tracked_guest_pid(local_instance.id.as_str())
            .expect("storage pid");
        assert_ne!(source_pid, local_pid);
        let before_job = crate::registry::registry_for_job(&state)
            .await
            .expect("job registry");
        let before_source = before_job.get(source_id).expect("job lookup before reload");
        assert_eq!(before_source.guest_pid(), Some(source_pid));
        let before_api = state
            .sources
            .read()
            .await
            .get(source_id)
            .expect("api lookup before reload");
        assert_eq!(before_api.guest_pid(), Some(source_pid));
        let before_storage = state
            .destinations
            .read()
            .await
            .plugin_session(&local_key, bookclerk_plugin_host::OPERATOR_ACCOUNT)
            .expect("storage session before reload");
        assert_eq!(before_storage.guest_pid(), Some(local_pid));
        let local_backend = state
            .destinations
            .read()
            .await
            .local()
            .expect("local backend");
        local_backend
            .put(
                "marker.txt",
                bytes::Bytes::from_static(b"phase2-root"),
                bookclerk_storage::ObjectMeta::default(),
            )
            .await
            .expect("put into instance root");
        assert!(
            file_tree_contains(&local_root, b"phase2-root"),
            "instance root {} did not receive the put",
            local_root.display()
        );
        assert!(
            !file_tree_contains(&files_path.join("Audiobooks"), b"phase2-root"),
            "put landed in the TOML default root"
        );

        let same_key_stops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let deployed_stops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let unowned_stops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        state
            .integrations
            .write()
            .await
            .register(Arc::new(CountingIntegration {
                id: "transitional-same-key",
                key: source_key.clone(),
                instance: None,
                stops: Arc::clone(&same_key_stops),
            }));
        state
            .integrations
            .write()
            .await
            .register(Arc::new(CountingIntegration {
                id: "deployed-integration",
                key: source_key.clone(),
                instance: Some(source_id.to_string()),
                stops: Arc::clone(&deployed_stops),
            }));
        state
            .integrations
            .write()
            .await
            .register(Arc::new(CountingIntegration {
                id: "transitional",
                key: "transitional-integration".into(),
                instance: None,
                stops: Arc::clone(&unowned_stops),
            }));
        state
            .sources
            .write()
            .await
            .register(Arc::new(KeyOnlySource {
                key: source_key.clone(),
            }));
        assert!(state
            .sources
            .read()
            .await
            .get(&source_key)
            .expect("legacy key")
            .plugin_instance_id()
            .is_none());

        crate::api::reload_daemon_config(&state)
            .await
            .expect("reload");

        let after_job = crate::registry::registry_for_job(&state)
            .await
            .expect("job registry after reload");
        let after_source = after_job.get(source_id).expect("job lookup after reload");
        assert_eq!(after_source.plugin_instance_id(), Some(source_id));
        assert_eq!(after_source.guest_pid(), Some(source_pid));
        let after_api = state
            .sources
            .read()
            .await
            .get(source_id)
            .expect("api lookup after reload");
        assert_eq!(after_api.guest_pid(), Some(source_pid));
        let after_storage = state
            .destinations
            .read()
            .await
            .plugin_session(&local_key, bookclerk_plugin_host::OPERATOR_ACCOUNT)
            .expect("storage session after reload");
        assert_eq!(after_storage.guest_pid(), Some(local_pid));
        assert!(state.destinations.read().await.local().is_some());
        assert!(
            bookclerk_plugin_host::DeploymentRuntime::guest_still_running(
                runtime.as_ref(),
                source_id
            )
            .await
        );
        let by_key = state
            .sources
            .read()
            .await
            .get(&source_key)
            .expect("plugin key reaches the deployed source");
        assert_eq!(by_key.plugin_instance_id(), Some(source_id));
        assert_eq!(by_key.guest_pid(), Some(source_pid));
        assert_eq!(same_key_stops.load(Ordering::SeqCst), 1);
        assert_eq!(deployed_stops.load(Ordering::SeqCst), 0);
        assert_eq!(unowned_stops.load(Ordering::SeqCst), 1);
        let kept = state
            .integrations
            .read()
            .await
            .get(&source_key)
            .expect("deployed integration");
        assert_eq!(kept.plugin_instance_id(), Some(source_id));
        assert!(state
            .integrations
            .read()
            .await
            .get("transitional-same-key")
            .is_none());
        assert!(state
            .integrations
            .read()
            .await
            .get("transitional-integration")
            .is_none());

        crate::api::reload_daemon_config(&state)
            .await
            .expect("second reload");
        assert_eq!(
            state
                .sources
                .read()
                .await
                .get(&source_key)
                .expect("plugin key after second reload")
                .guest_pid(),
            Some(source_pid)
        );
        assert_eq!(
            state
                .destinations
                .read()
                .await
                .plugin_session(&local_key, bookclerk_plugin_host::OPERATOR_ACCOUNT)
                .expect("storage after second reload")
                .guest_pid(),
            Some(local_pid)
        );
        assert_eq!(deployed_stops.load(Ordering::SeqCst), 0);

        super::reconcile_deployments(&state)
            .await
            .expect("reconcile after reload");
        assert_eq!(runtime.tracked_guest_pid(source_id), Some(source_pid));
        assert_eq!(
            runtime.tracked_guest_pid(local_instance.id.as_str()),
            Some(local_pid)
        );
        assert_eq!(
            state
                .sources
                .read()
                .await
                .get(source_id)
                .expect("api lookup after second tick")
                .guest_pid(),
            Some(source_pid)
        );

        let second_local = create_plugin_instance(&store, &actor, &local_key)
            .await
            .expect("second local instance");
        let mut second_settings = std::collections::BTreeMap::new();
        second_settings.insert(
            "prefix".into(),
            bookclerk_library::control_plane::SettingValue::String("other".into()),
        );
        second_settings.insert(
            "root".into(),
            bookclerk_library::control_plane::SettingValue::String(
                files_path.join("other-audiobooks").display().to_string(),
            ),
        );
        import_instance_config_if_absent(
            &store,
            &actor,
            &second_local.id,
            InstancePackagePolicy::Generic,
            &PluginInstanceConfigV1 {
                settings: second_settings,
                secret_refs: Vec::new(),
            },
            "import-second-local",
        )
        .await
        .expect("import second local");
        let second_deployment =
            ensure_plugin_deployment(&store, &actor, &second_local.id, &session.host.host_id)
                .await
                .expect("second local deployment");
        super::reconcile_deployments(&state)
            .await
            .expect("second local reconcile");
        let second_obs = load_observation(
            &store,
            &second_deployment.deployment_id,
            &session.host.host_id,
        )
        .await
        .unwrap()
        .expect("second observation");
        assert_eq!(
            second_obs.status,
            DeploymentStatus::Error,
            "{}",
            second_obs.detail
        );
        assert!(
            second_obs.detail.contains("another local destination"),
            "{}",
            second_obs.detail
        );
        assert_eq!(
            runtime.tracked_guest_pid(local_instance.id.as_str()),
            Some(local_pid)
        );
        assert!(runtime
            .tracked_guest_pid(second_local.id.as_str())
            .is_none());

        let stops_before = deployed_stops.load(Ordering::SeqCst);
        runtime
            .retire_plugin_instance(local_instance.id.as_str())
            .await;
        assert!(runtime
            .tracked_guest_pid(local_instance.id.as_str())
            .is_none());
        assert!(state.destinations.read().await.local().is_none());
        assert!(
            !bookclerk_plugin_host::DeploymentRuntime::guest_still_running(
                runtime.as_ref(),
                local_instance.id.as_str()
            )
            .await
        );
        runtime.retire_plugin_instance(source_id).await;
        assert!(deployed_stops.load(Ordering::SeqCst) > stops_before);
        assert!(runtime.tracked_guest_pid(source_id).is_none());

        for pid in [source_pid, local_pid] {
            kill_pid(pid);
        }
    }

    #[tokio::test]
    async fn daemon_reload_retires_transitional_guest_when_deployment_is_added() {
        use std::path::PathBuf;
        use std::sync::atomic::Ordering;
        use std::sync::Arc;

        use bookclerk_config::Isolation;
        use bookclerk_library::control_plane::{
            create_plugin_instance, ensure_plugin_deployment, import_instance_config_if_absent,
            load_observation, ConfigActor, DeploymentStatus, InstancePackagePolicy,
            PluginInstanceConfigV1,
        };

        let files = tempfile::tempdir().expect("files");
        let files_path = files.path();
        std::fs::write(
            files_path.join("config.toml"),
            "[plugins]\nisolation = \"off\"\n\n[daemon.auth]\nenabled = false\n",
        )
        .unwrap();
        let source_toml = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../bookclerk-plugins/optional/source-graphicaudio/plugin.toml");
        let package_key = stage_graphicaudio_package(files_path, &source_toml);
        let config = Config::load(
            Some(files_path.to_path_buf()),
            Some(files_path.join("config.toml")),
        )
        .unwrap();
        assert_eq!(config.plugins.isolation, Isolation::Off);
        install_staged_archive(&config, files_path, &package_key);
        let plugin_key = bookclerk_plugin_host::graphicaudio_plugin_key(&config)
            .expect("discover")
            .expect("graphicaudio key");

        let manifest = bookclerk_plugin_host::PluginManifest::parse(
            &std::fs::read_to_string(&source_toml).unwrap(),
        )
        .unwrap();
        let mut grant = bookclerk_plugin_host::consent_request_alias(&manifest);
        grant.plugin_key = plugin_key.clone();
        let mut grants = bookclerk_plugin_host::PluginGrantStore::default();
        grants.upsert(grant);
        grants.save(files_path).unwrap();

        let db = bookclerk_plugin_database_sqlite::open(&files_path.join("library.db"))
            .await
            .expect("sqlite");
        bookclerk_library::apply_host_schema(&db)
            .await
            .expect("schema");
        let store = bookclerk_library::LibraryStore::from_connection(db);
        let session = bootstrap_control_plane(
            &store,
            files_path,
            None,
            &bookclerk_config::EventsConfig::default(),
        )
        .await
        .expect("bootstrap");
        let state = control_plane_test_state(store.clone(), config);
        let loaded = bookclerk_plugin_host::load_sources_skipping(
            &state.config.read().await.clone(),
            &bookclerk_plugin_host::SessionServices::from_outbox(Some(&store)),
            &std::collections::BTreeSet::new(),
        )
        .await
        .expect("transitional sources");
        let transitional_pid = {
            let source = loaded.get(&plugin_key).unwrap_or_else(|| {
                panic!(
                    "transitional guest missing; registered: {:?}",
                    loaded
                        .all()
                        .iter()
                        .map(|source| source.plugin_key().to_string())
                        .collect::<Vec<_>>()
                )
            });
            assert!(source.plugin_instance_id().is_none());
            assert_eq!(source.plugin_key(), plugin_key);
            source.guest_pid().expect("transitional pid")
        };
        *state.sources.write().await = loaded;

        let transitional_stops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        state
            .integrations
            .write()
            .await
            .register(Arc::new(CountingIntegration {
                id: "transitional-integration",
                key: plugin_key.clone(),
                instance: None,
                stops: Arc::clone(&transitional_stops),
            }));
        state
            .integrations
            .read()
            .await
            .start_all(bookclerk_integrations::IntegrationContext::default())
            .await
            .expect("start transitional integration");

        let actor = ConfigActor::Bootstrap;
        let instance = create_plugin_instance(&store, &actor, &plugin_key)
            .await
            .expect("instance");
        let mut settings = std::collections::BTreeMap::new();
        settings.insert(
            "access".into(),
            bookclerk_library::control_plane::SettingValue::String("device".into()),
        );
        import_instance_config_if_absent(
            &store,
            &actor,
            &instance.id,
            InstancePackagePolicy::GraphicAudio,
            &PluginInstanceConfigV1 {
                settings,
                secret_refs: Vec::new(),
            },
            "import-transition-ga",
        )
        .await
        .expect("import");
        let deployment =
            ensure_plugin_deployment(&store, &actor, &instance.id, &session.host.host_id)
                .await
                .expect("deployment");
        let instance_id = instance.id.as_str().to_string();

        crate::api::reload_daemon_config(&state)
            .await
            .expect("reload");
        assert!(
            state.sources.read().await.get(&plugin_key).is_none(),
            "transitional source must not survive reload"
        );
        assert!(state.sources.read().await.get(&instance_id).is_none());
        assert_eq!(transitional_stops.load(Ordering::SeqCst), 1);
        assert!(state.integrations.read().await.get(&plugin_key).is_none());

        super::reconcile_deployments(&state)
            .await
            .expect("reconcile deployed guest");
        let obs = load_observation(&store, &deployment.deployment_id, &session.host.host_id)
            .await
            .unwrap()
            .expect("observation");
        assert_eq!(obs.status, DeploymentStatus::Healthy, "{}", obs.detail);
        let deployed_pid = state
            .deployment_runtime()
            .tracked_guest_pid(&instance_id)
            .expect("deployed pid");
        assert_ne!(deployed_pid, transitional_pid);
        let by_instance = state
            .sources
            .read()
            .await
            .get(&instance_id)
            .expect("instance lookup");
        assert_eq!(by_instance.plugin_instance_id(), Some(instance_id.as_str()));
        assert_eq!(by_instance.guest_pid(), Some(deployed_pid));
        let by_key = state
            .sources
            .read()
            .await
            .get(&plugin_key)
            .expect("plugin key lookup");
        assert_eq!(by_key.plugin_instance_id(), Some(instance_id.as_str()));
        assert_eq!(by_key.guest_pid(), Some(deployed_pid));
        assert!(state.integrations.read().await.get(&plugin_key).is_none());
        assert_eq!(transitional_stops.load(Ordering::SeqCst), 1);

        for pid in [transitional_pid, deployed_pid] {
            kill_pid(pid);
        }
    }
}
