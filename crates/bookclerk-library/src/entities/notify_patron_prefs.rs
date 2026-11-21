//! SeaORM entity for `notify_patron_prefs`: one patron's choice for one
//! patron notification type.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "notify_patron_prefs")]
pub struct Model {
    /// The patron.
    #[sea_orm(primary_key, auto_increment = false)]
    pub user_id: i64,
    /// Qualified patron notification type.
    #[sea_orm(primary_key, auto_increment = false)]
    pub notification_type: String,
    /// 1 when the patron wants it outside the inbox.
    pub enabled: i64,
    /// RFC 3339 time of the last write.
    pub updated_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
