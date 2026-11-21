//! SeaORM entity for `notify_routes`: administrators' routing of one
//! operational notification type to one channel destination.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "notify_routes")]
pub struct Model {
    /// Qualified notification type (`test`, `<plugin>:<type>`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub notification_type: String,
    /// Channel destination the type goes to.
    #[sea_orm(primary_key, auto_increment = false)]
    pub destination_id: String,
    /// Lowest severity sent (`info`, `warning`, `critical`).
    pub min_severity: String,
    /// RFC 3339 creation time.
    pub created_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
