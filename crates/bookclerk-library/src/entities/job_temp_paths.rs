//! SeaORM entity for scratch directories associated with a durable job.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "job_temp_paths")]
pub struct Model {
    /// Surrogate primary key assigned by the database.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// Owning job id (`jobs.id`).
    pub job_id: String,
    /// Absolute filesystem path of a scratch directory or file.
    pub path: String,
    /// RFC 3339 timestamp when the path was registered.
    pub created_at: String,
    /// Bytes reserved against `jobs.temp_quota_bytes` for this path.
    pub reserved_bytes: i64,
}

/// Declared SeaORM relations (queried by `job_id`, not modeled as an FK edge).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
