//! SeaORM entity for `notify_inbox_routes`: administrators' routing of one
//! operational notification type to the built-in "In app" target, the
//! inbox of every active Owner and Administrator.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "notify_inbox_routes")]
pub struct Model {
    /// Qualified notification type (`test`, `<plugin>:<type>`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub notification_type: String,
    /// Lowest severity put in the inbox (`info`, `warning`, `critical`).
    pub min_severity: String,
    /// RFC 3339 creation time.
    pub created_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
