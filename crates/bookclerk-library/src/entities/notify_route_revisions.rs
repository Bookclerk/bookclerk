//! SeaORM entity for `notify_route_revisions`: a fresh revision written with
//! every replacement of one operational type's routes, so routing can tell
//! that its separate reads of the channel routes and the "In app" route saw
//! one configuration.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "notify_route_revisions")]
pub struct Model {
    /// Qualified notification type (`test`, `<plugin>:<type>`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub notification_type: String,
    /// Random UUID written by the newest replacement; never reused, so two
    /// equal reads mean no replacement landed between them.
    pub revision: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
