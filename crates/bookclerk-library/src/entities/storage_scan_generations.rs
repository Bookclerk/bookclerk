//! Ownership row for one durable storage-scan generation.
//!
//! `storage_scan_rows` are keyed only by `scan_id`. This row binds that id to
//! the storage instance and, when the scan runs inside a job, the job that
//! owns it. Cleanup uses the row to tell a live scan from an abandoned one.

use sea_orm::entity::prelude::*;

/// One scan generation.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "storage_scan_generations")]
pub struct Model {
    /// Globally unique scan id.
    #[sea_orm(primary_key, auto_increment = false)]
    pub scan_id: String,
    /// Storage instance id (`StorageBackend::instance_id`) this generation lists.
    pub instance_id: String,
    /// Job id when the scan is fenced. Empty when the scan has no job.
    pub job_id: String,
    /// RFC 3339 heartbeat. Page ingestion updates this.
    pub updated_at: String,
    /// 1 after the list phase finished and the inventory can be adopted.
    pub completed: i64,
}

/// No declared relations.
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
