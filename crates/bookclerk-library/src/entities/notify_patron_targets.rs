//! SeaORM entity for `notify_patron_targets`: a personal destination one
//! patron chose for one patron notification type.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "notify_patron_targets")]
pub struct Model {
    /// The patron.
    #[sea_orm(primary_key, auto_increment = false)]
    pub user_id: i64,
    /// Qualified patron notification type.
    #[sea_orm(primary_key, auto_increment = false)]
    pub notification_type: String,
    /// One of the patron's personal destinations.
    #[sea_orm(primary_key, auto_increment = false)]
    pub destination_id: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
