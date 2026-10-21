//! SeaORM entity for `job_secret_handoffs`: which store-account secrets a
//! Host was handed with a job, per lease generation.
//!
//! Recorded when the claim hands the secrets over and read when the Host
//! writes a refreshed token back, so a write-back is accepted only for a
//! secret that went out under that exact fence. Carries no secret material.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "job_secret_handoffs")]
pub struct Model {
    /// Job the secret was handed with (`jobs.id`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub job_id: String,
    /// Lease generation of the claim that handed it.
    #[sea_orm(primary_key, auto_increment = false)]
    pub lease_generation: i64,
    /// Secret kind (`source_auth`, ...).
    #[sea_orm(primary_key, auto_increment = false)]
    pub kind: String,
    /// Plugin key that owns the secret.
    #[sea_orm(primary_key, auto_increment = false)]
    pub provider: String,
    /// Store account id.
    #[sea_orm(primary_key, auto_increment = false)]
    pub account_id: String,
    /// Logical secret name.
    #[sea_orm(primary_key, auto_increment = false)]
    pub name: String,
    /// Node the secret was handed to.
    pub node_id: String,
    /// RFC 3339 time of the handoff.
    pub handed_at: String,
}

/// Declared SeaORM relations (queried by `job_id`, not modeled as an FK edge).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
