//! SeaORM entity for `notify_inbox`: a patron-audience notification in its
//! patron's in-app inbox.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "notify_inbox")]
pub struct Model {
    /// The patron.
    #[sea_orm(primary_key, auto_increment = false)]
    pub user_id: i64,
    /// The notification.
    #[sea_orm(primary_key, auto_increment = false)]
    pub notification_id: String,
    /// RFC 3339 time the patron read it; `None` while unread.
    pub read_at: Option<String>,
    /// RFC 3339 time it arrived.
    pub created_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
