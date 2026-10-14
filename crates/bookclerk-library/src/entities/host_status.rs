//! SeaORM entity for `host_status`: each host's latest daemon heartbeat.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "host_status")]
pub struct Model {
    /// Reporting host.
    #[sea_orm(primary_key, auto_increment = false)]
    pub host_id: String,
    /// Daemon process incarnation.
    pub incarnation: String,
    /// RFC 3339 time the daemon process started.
    pub started_at: String,
    /// RFC 3339 time of this heartbeat.
    pub heartbeat_at: String,
    /// Same time in Unix milliseconds (liveness arithmetic).
    pub heartbeat_ms: i64,
    /// Heartbeat interval the daemon promised, in milliseconds.
    pub interval_ms: i64,
    /// Detected CPU capacity in millicores (machine or cgroup, whichever is tighter).
    pub cpu_millicores: i64,
    /// Detected memory in bytes; empty when unknown.
    pub memory_bytes: Option<i64>,
    /// CPU the scheduler may commit here, in millicores.
    pub cpu_budget_millicores: i64,
    /// Memory the scheduler may commit here; empty when memory is unknown.
    pub memory_budget_bytes: Option<i64>,
    /// `1` unless an operator drained the host.
    pub schedulable: i64,
    /// Measured CPU in use, in millicores; empty until two samples exist.
    pub cpu_used_millicores: Option<i64>,
    /// Measured memory available to new work.
    pub memory_available_bytes: Option<i64>,
    /// Free bytes on the files directory's filesystem.
    pub files_dir_free_bytes: Option<i64>,
    /// Sum of running guests' CPU ceilings, in millicores.
    pub reserved_cpu_millicores: i64,
    /// Sum of running guests' memory ceilings.
    pub reserved_memory_bytes: i64,
    /// Guests running on the host.
    pub running_guests: i64,
    /// Recent online fraction in parts per million.
    pub availability_ppm: i64,
    /// Unix milliseconds the current unbroken online stretch began.
    pub online_since_ms: i64,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
