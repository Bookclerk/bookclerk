//! SeaORM entity for `plugin_placements`: the scheduler's input per instance.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "plugin_placements")]
pub struct Model {
    /// Placed instance.
    #[sea_orm(primary_key, auto_increment = false)]
    pub plugin_instance_id: String,
    /// Hosts that should run the instance.
    pub replicas: i64,
    /// JSON array of label constraints every chosen host must satisfy.
    pub constraints_json: String,
    /// Compare-and-swap revision of the operator's intent.
    pub revision: i64,
    /// RFC 3339 time of the last intent write.
    pub updated_at: String,
    /// Audit identity of the last intent writer.
    pub updated_by: String,
    /// Replicas the last scheduler pass placed.
    pub placed: i64,
    /// Why the last pass placed fewer than `replicas`; empty when satisfied.
    pub detail: String,
    /// RFC 3339 time of the last scheduler pass that changed the status.
    pub scheduled_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
