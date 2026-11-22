//! SeaORM entity for `notify_patron_offers`: a notifier instance patrons
//! may use for one patron notification type.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "notify_patron_offers")]
pub struct Model {
    /// Qualified patron notification type.
    #[sea_orm(primary_key, auto_increment = false)]
    pub notification_type: String,
    /// Notifier instance offered for it.
    #[sea_orm(primary_key, auto_increment = false)]
    pub plugin_instance_id: String,
    /// 1 when a patron who has not chosen gets the type on their personal
    /// destinations of this instance.
    pub default_on: i64,
    /// RFC 3339 creation time.
    pub created_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
