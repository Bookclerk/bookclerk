//! Plugin cron fires: exactly one `plugin_scheduled` job per schedule fire,
//! cluster-wide.
//!
//! Every node that runs a plugin instance with `[triggers] crons` ticks its
//! schedules. A `plugin_cron_cursors` row per `(instance, cron)` records the
//! last fire time that was claimed. Claiming a fire is one canonical batch:
//! a compare-and-swap that moves the cursor from the value the node read to
//! the new fire time and stamps a fresh claim token, then an `INSERT OR
//! IGNORE … SELECT` of the job that only selects a row while the cursor
//! still carries that token. Two nodes that race for one fire therefore
//! enqueue it once, and a node that crashes after the batch has already
//! committed both the cursor and the job, so a fire is never lost.
//!
//! The job's dedup key is per `(instance, cron)` (see
//! [`crate::plugin_scheduled_dedup_key`]), so a fire that comes due while the
//! previous fire of the same schedule is still queued or running is skipped:
//! the cursor still advances, and the insert is ignored by the active-dedup
//! unique index.
//!
//! Missed fires coalesce. A cursor behind by several fire times claims only
//! the newest one at or before now; a new cursor starts at the newest past
//! fire, so installing or deploying a plugin never fires it at once.
//!
//! Cursors are removed with their instance (`delete_plugin_instance`).

use chrono::Utc;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use uuid::Uuid;

use super::batch::{self, exec, int, request, text};
use crate::entities::plugin_cron_cursors;
use crate::error::{LibraryError, Result};
use crate::models::{plugin_scheduled_dedup_key, JobKind, JobPayload, JobState, JobTrigger};
use crate::store::LibraryStore;

/// Failure attempts a cron fire gets before it is terminal (the first run
/// plus two retries with the queue's backoff).
pub const CRON_FIRE_MAX_ATTEMPTS: i64 = 3;

/// One `plugin_cron_cursors` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronCursor {
    /// Plugin instance whose schedule this is.
    pub plugin_instance_id: String,
    /// Normalized cron source.
    pub cron: String,
    /// Last claimed (or, for a fresh cursor, last skipped) fire time in Unix
    /// milliseconds.
    pub last_fire_ms: i64,
}

/// What [`claim_cron_fire`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronFireClaim {
    /// This call moved the cursor and admitted the fire as job `job_id`, or
    /// moved the cursor while an earlier fire of the schedule was still
    /// active (`job_id` is `None`; the fire is skipped).
    Claimed {
        /// The new job, or `None` when the fire coalesced onto an active one.
        job_id: Option<String>,
    },
    /// The cursor was not at `expected_last_ms` (another node claimed the
    /// fire, or this node's view is stale). Re-read with
    /// [`load_cron_cursor`].
    Lost,
}

/// Reads one cursor.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn load_cron_cursor(
    store: &LibraryStore,
    plugin_instance_id: &str,
    cron: &str,
) -> Result<Option<CronCursor>> {
    let row = plugin_cron_cursors::Entity::find()
        .filter(plugin_cron_cursors::Column::PluginInstanceId.eq(plugin_instance_id))
        .filter(plugin_cron_cursors::Column::Cron.eq(cron))
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    Ok(row.map(|row| CronCursor {
        plugin_instance_id: row.plugin_instance_id,
        cron: row.cron,
        last_fire_ms: row.last_fire_ms,
    }))
}

/// Creates the cursor at `initial_last_ms` unless one exists, and returns
/// the stored cursor.
///
/// # Arguments
///
/// * `initial_last_ms` - The newest past fire time, so a new schedule waits
///   for its next fire instead of firing at once.
///
/// # Errors
///
/// Returns an error when the write or the read-back fails.
pub async fn ensure_cron_cursor(
    store: &LibraryStore,
    plugin_instance_id: &str,
    cron: &str,
    initial_last_ms: i64,
) -> Result<CronCursor> {
    if let Some(cursor) = load_cron_cursor(store, plugin_instance_id, cron).await? {
        return Ok(cursor);
    }
    let operation_id = format!("cron-cursor-{}", Uuid::new_v4());
    batch::execute_host_batch(
        store,
        request(
            &operation_id,
            vec![exec(
                "INSERT OR IGNORE INTO plugin_cron_cursors (
                    plugin_instance_id, cron, last_fire_ms, claim, updated_at
                 ) VALUES (?, ?, ?, '', ?)",
                vec![
                    text(plugin_instance_id),
                    text(cron),
                    int(initial_last_ms),
                    text(&Utc::now().to_rfc3339()),
                ],
            )],
        ),
    )
    .await?;
    load_cron_cursor(store, plugin_instance_id, cron)
        .await?
        .ok_or_else(|| {
            LibraryError::Other(anyhow::anyhow!(
                "cron cursor {plugin_instance_id} `{cron}` missing after insert"
            ))
        })
}

/// Claims the fire at `fire_ms` for `(plugin_instance_id, cron)` and admits
/// its `plugin_scheduled` job in the same batch.
///
/// # Arguments
///
/// * `expected_last_ms` - The cursor value this node read; the claim is a
///   compare-and-swap on it.
/// * `fire_ms` - The fire time to claim; must be later than
///   `expected_last_ms`.
///
/// # Returns
///
/// [`CronFireClaim::Claimed`] when this call moved the cursor, otherwise
/// [`CronFireClaim::Lost`].
///
/// # Errors
///
/// Returns an error when `fire_ms` does not advance the cursor, or when the
/// batch or the read-back fails.
pub async fn claim_cron_fire(
    store: &LibraryStore,
    plugin_instance_id: &str,
    cron: &str,
    expected_last_ms: i64,
    fire_ms: i64,
) -> Result<CronFireClaim> {
    if fire_ms <= expected_last_ms {
        return Err(LibraryError::Invalid(format!(
            "cron fire {fire_ms} does not advance the cursor past {expected_last_ms}"
        )));
    }
    let claim = Uuid::new_v4().to_string();
    let job_id = format!("{}-{}", JobKind::PluginScheduled.as_str(), Uuid::new_v4());
    let payload = JobPayload {
        plugin_instance_id: Some(plugin_instance_id.to_string()),
        requires: vec![plugin_instance_id.to_string()],
        trigger: JobTrigger::Scheduler,
        cron: Some(cron.to_string()),
        scheduled_time_ms: Some(fire_ms),
        ..JobPayload::default()
    };
    let payload_json = serde_json::to_string(&payload)
        .map_err(|err| LibraryError::Other(anyhow::anyhow!("cron job payload: {err}")))?;
    let dedup_key = plugin_scheduled_dedup_key(plugin_instance_id, cron);
    let now = Utc::now().to_rfc3339();
    let reply = batch::execute_host_batch(
        store,
        request(
            &format!("cron-fire-{claim}"),
            vec![
                exec(
                    "UPDATE plugin_cron_cursors
                        SET last_fire_ms = ?, claim = ?, updated_at = ?
                      WHERE plugin_instance_id = ? AND cron = ? AND last_fire_ms = ?",
                    vec![
                        int(fire_ms),
                        text(&claim),
                        text(&now),
                        text(plugin_instance_id),
                        text(cron),
                        int(expected_last_ms),
                    ],
                ),
                exec(
                    "INSERT OR IGNORE INTO jobs (
                        id, kind, state, priority, resource_class, payload,
                        attempt_count, max_attempts, run_after, dedup_key,
                        cancel_requested, created_at, updated_at, lease_generation
                     ) SELECT ?, ?, ?, 0, ?, ?, 0, ?, ?, ?, 0, ?, ?, 0
                         FROM plugin_cron_cursors
                        WHERE plugin_instance_id = ? AND cron = ? AND claim = ?",
                    vec![
                        text(&job_id),
                        text(JobKind::PluginScheduled.as_str()),
                        text(JobState::Pending.as_str()),
                        text(JobKind::PluginScheduled.resource_class().as_str()),
                        text(&payload_json),
                        int(CRON_FIRE_MAX_ATTEMPTS),
                        text(&now),
                        text(&dedup_key),
                        text(&now),
                        text(&now),
                        text(plugin_instance_id),
                        text(cron),
                        text(&claim),
                    ],
                ),
            ],
        ),
    )
    .await?;
    if batch::rows_affected(&reply, 0) == 0 {
        return Ok(CronFireClaim::Lost);
    }
    let inserted = batch::rows_affected(&reply, 1) > 0;
    Ok(CronFireClaim::Claimed {
        job_id: inserted.then_some(job_id),
    })
}
