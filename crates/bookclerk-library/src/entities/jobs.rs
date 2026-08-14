//! SeaORM entity for the durable daemon `jobs` table.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "jobs")]
pub struct Model {
    /// Stable job id (`scan-{uuid}`, `acquire-{uuid}`, `listen_sync-{uuid}`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// Job kind (`scan`, `acquire`, `listen_sync`).
    pub kind: String,
    /// Lifecycle state (`pending`, `running`, `succeeded`, `failed`, `cancelled`).
    pub state: String,
    /// Higher values are claimed first when several jobs are ready.
    pub priority: i64,
    /// Concurrency class (`network`, `media`, `transcription`, `indexing`).
    pub resource_class: String,
    /// JSON payload (`account`, `title`, `trigger`).
    pub payload: String,
    /// Human-readable progress string shown in the API and UI.
    pub progress: Option<String>,
    /// Number of times a worker has claimed this row.
    pub attempt_count: i64,
    /// Maximum claims before a failure becomes terminal.
    pub max_attempts: i64,
    /// RFC 3339 time after which a pending job may be claimed (backoff / delay).
    pub run_after: String,
    /// Worker id that currently holds the lease, when `state = running`.
    pub lease_owner: Option<String>,
    /// RFC 3339 lease expiry; stale running rows are reclaimed after this.
    pub lease_expires_at: Option<String>,
    /// Idempotency key unique among pending/running rows of the same work.
    pub dedup_key: String,
    /// Structured error kind (`queue_full` is admission-only; execution uses
    /// `handler`, `cancelled`, `orphaned_after_restart`, `max_attempts`).
    pub error_kind: Option<String>,
    /// Operator-facing error text when the job failed or was cancelled.
    pub error_message: Option<String>,
    /// Cooperative cancel flag (`0` / `1`); running workers check between steps.
    pub cancel_requested: i64,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: String,
    /// RFC 3339 timestamp when a worker first claimed the row.
    pub started_at: Option<String>,
    /// RFC 3339 timestamp when the row reached a terminal state.
    pub finished_at: Option<String>,
    /// Per-claim generation used to fence heartbeat and finalization.
    pub lease_generation: i64,
}

/// Declared SeaORM relations (temp paths are queried by `job_id`, not modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
