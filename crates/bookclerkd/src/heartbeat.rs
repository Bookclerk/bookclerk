//! This node's periodic heartbeat into `node_status`.
//!
//! Every [`NODE_HEARTBEAT_INTERVAL`] the daemon samples detected capacity and
//! measured use, and the library adds this node's budget (`node.runtime`),
//! the reservations of its running guests, and the availability average.
//! Other nodes and the placement scheduler read the row from the shared
//! database; nodes never contact each other.
//!
//! Each beat also re-reads the image caches' retention limits (`core.cache`
//! and this node's override) and hands them to the cover service. When it
//! says a sweep is due, the beat queues a `cache_sweep` job pinned to this
//! node (see [`crate::cache_jobs`]). The row carries what the caches hold,
//! their budget and when they were last swept.
//!
//! The recorded row also sets how many job slots this machine has for the
//! libraries it is a branch of ([`crate::library_link::capacity`]).
//!
//! A beat is written only while this node's `nodes` row exists. A shared
//! node removed from the library while it was cut off from the database
//! (`control_plane::remove_shared_node` refuses a live one) would
//! otherwise come back as a status row with no node, deployments or
//! subscriber registrations behind it, serving while missing from
//! library administration. Instead each beat logs an error saying the node
//! was removed, and the node writes no status and queues no node-pinned
//! work until it is stopped, or restarted, which registers it again.

use std::sync::Arc;

use bookclerk_library::control_plane::{
    load_cache_settings, load_node, load_node_runtime, load_or_create_node_identity,
    record_node_heartbeat, NodeRuntimeSettingsV1, NodeSampler, NodeStatus, NODE_HEARTBEAT_INTERVAL,
};
use bookclerk_library::{LibraryError, LibraryStore};
use tracing::warn;

use crate::api::AppState;

/// Starts the heartbeat loop. The first heartbeat is written at once.
pub fn spawn_heartbeat(state: Arc<AppState>) {
    tokio::spawn(async move {
        let files_dir = state.config.read().await.paths().files_dir.clone();
        let mut sampler = Some(NodeSampler::new(&files_dir));
        loop {
            if let Some(taken) = sampler.take() {
                let (back, status) = beat(&state, taken).await;
                sampler = Some(back);
                if let Err(err) = status {
                    warn!(error = %err, "node heartbeat failed");
                }
            }
            tokio::time::sleep(NODE_HEARTBEAT_INTERVAL).await;
        }
    });
}

/// Samples on a blocking thread, then records the heartbeat.
async fn beat(
    state: &AppState,
    mut sampler: NodeSampler,
) -> (NodeSampler, anyhow::Result<NodeStatus>) {
    let (sampler, mut heartbeat) = match tokio::task::spawn_blocking(move || {
        let heartbeat = sampler.sample();
        (sampler, heartbeat)
    })
    .await
    {
        Ok(pair) => pair,
        Err(err) => {
            let files_dir = state.config.read().await.paths().files_dir.clone();
            return (
                NodeSampler::new(&files_dir),
                Err(anyhow::anyhow!("heartbeat sample panicked: {err}")),
            );
        }
    };
    let files_dir = state.config.read().await.paths().files_dir.clone();
    let node_id = match load_or_create_node_identity(&files_dir) {
        Ok(identity) => identity.node_id,
        Err(err) => return (sampler, Err(err.into())),
    };
    let library = state.library_snapshot().await;
    if let Err(err) = refuse_removed_node(&library, &node_id).await {
        return (sampler, Err(err));
    }
    let limits = match cache_limits(&library, &node_id).await {
        Ok(limits) => limits,
        Err(err) => {
            // The disk check and the hourly sweep still run.
            warn!(error = %err, "cache limits unavailable; keeping the previous ones");
            state.covers.limits()
        }
    };
    let due = state.covers.maintain(limits).await;
    heartbeat.cache = Some(state.covers.usage());
    let status = record_node_heartbeat(&library, &node_id, &heartbeat)
        .await
        .map_err(anyhow::Error::from);
    if due {
        if status.is_ok() {
            // Queued only once this beat is on record, so the requirements
            // sweep never sees the job before it sees its node alive.
            crate::cache_jobs::queue_cache_sweep(state, &node_id).await;
        } else {
            state.covers.sweep_in_background();
        }
    }
    if let Ok(recorded) = &status {
        // Same ordering as the sweep: a node-pinned job only after the beat.
        crate::search_index::queue_rebuild_if_missing(state, &node_id).await;
        crate::library_index::queue_rebuild_if_missing(state).await;
        // The libraries this node is a branch of share what its budget
        // fits now.
        state.capacity.record_status(recorded);
    }
    crate::cache_jobs::sweep_if_overdue(state);
    (sampler, status)
}

/// Refuses a beat for `node_id` when its `nodes` row is gone: it was removed
/// from the library while it was cut off (see the module docs). Logs an
/// error saying so.
///
/// # Errors
///
/// Returns an error when the row is gone, or cannot be read.
async fn refuse_removed_node(library: &LibraryStore, node_id: &str) -> anyhow::Result<()> {
    match load_node(library, node_id).await {
        Ok(_) => Ok(()),
        Err(LibraryError::NotFound(_)) => {
            tracing::error!(
                node_id = %node_id,
                "this node was removed from the library while it was cut off; it writes no \
                 heartbeat and queues no work here. Stop Bookclerk on this machine, or restart \
                 it to register the node again"
            );
            Err(anyhow::anyhow!(
                "node {node_id} was removed from the library"
            ))
        }
        Err(err) => Err(err.into()),
    }
}

/// The image caches' retention limits on `node_id`: the library's
/// `core.cache` document and the node's budget override, each falling back
/// to its defaults when never written.
///
/// # Errors
///
/// A database read failure, or a stored document that is invalid.
pub(crate) async fn cache_limits(
    library: &LibraryStore,
    node_id: &str,
) -> Result<crate::covers::Limits, LibraryError> {
    let settings = load_cache_settings(library)
        .await?
        .map(|doc| doc.body)
        .unwrap_or_default();
    let node = match load_node_runtime(library, node_id).await {
        Ok(doc) => doc.body,
        Err(LibraryError::NotFound(_)) => NodeRuntimeSettingsV1::new(),
        Err(err) => return Err(err),
    };
    Ok(crate::covers::Limits::from_settings(&settings, &node))
}

#[cfg(test)]
mod tests {
    use bookclerk_library::control_plane::{
        update_cache_settings, update_node_runtime, ConfigActor, DEFAULT_CACHE_BUDGET_BYTES,
    };

    use super::*;

    /// A registered node beats; once its `nodes` row is gone (removed while
    /// it was cut off), its beats are refused rather than writing a status
    /// row with no node behind it.
    #[tokio::test]
    async fn a_removed_node_writes_no_heartbeat() {
        use sea_orm::{ColumnTrait as _, EntityTrait as _, QueryFilter as _};
        let db = bookclerk_plugin_database_sqlite::open_memory()
            .await
            .expect("sqlite");
        bookclerk_library::apply_host_schema(&db)
            .await
            .expect("schema");
        let store = LibraryStore::from_connection(db);
        let files = tempfile::tempdir().expect("files");
        let session = bookclerk_library::control_plane::bootstrap_control_plane(
            &store,
            files.path(),
            &files.path().join("cache"),
            None,
            &bookclerk_config::Config::default(),
        )
        .await
        .expect("bootstrap");
        let node_id = session.node.node_id;
        refuse_removed_node(&store, &node_id)
            .await
            .expect("a registered node beats");
        bookclerk_library::entities::nodes::Entity::delete_many()
            .filter(bookclerk_library::entities::nodes::Column::NodeId.eq(node_id.clone()))
            .exec(store.db())
            .await
            .expect("remove");
        let err = refuse_removed_node(&store, &node_id)
            .await
            .expect_err("a removed node is refused");
        assert!(err.to_string().contains("removed"), "{err}");
    }

    #[tokio::test]
    async fn cache_limits_follow_the_library_document_and_the_node_override() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = bookclerk_plugin_database_sqlite::open(&dir.path().join("library.db"))
            .await
            .expect("open");
        bookclerk_library::apply_host_schema(&db)
            .await
            .expect("schema");
        let store = LibraryStore::from_connection(db);
        let actor = ConfigActor::Operator { id: "test".into() };

        let defaults = cache_limits(&store, "node-a").await.expect("defaults");
        assert_eq!(defaults, crate::covers::Limits::default());
        assert_eq!(defaults.budget, DEFAULT_CACHE_BUDGET_BYTES);

        update_cache_settings(&store, &actor, |body| {
            body.set("cache.unviewed_days", "7")?;
            body.set("cache.budget_bytes", "67108864")
        })
        .await
        .expect("library");
        let library = cache_limits(&store, "node-a").await.expect("library");
        assert_eq!(library.budget, 64 << 20);
        assert_eq!(
            library.unviewed,
            std::time::Duration::from_secs(7 * 24 * 3600)
        );

        update_node_runtime(&store, &actor, "node-a", |runtime| {
            runtime.cache_budget_bytes = Some(32 << 20);
        })
        .await
        .expect("override");
        assert_eq!(
            cache_limits(&store, "node-a").await.expect("node").budget,
            32 << 20
        );
        assert_eq!(
            cache_limits(&store, "node-b").await.expect("other").budget,
            64 << 20,
            "another node keeps the library budget"
        );
    }
}
