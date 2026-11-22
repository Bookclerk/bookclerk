//! SeaORM entity for `notify_destinations`: where one notifier instance
//! sends, either a library channel or one patron's personal address.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "notify_destinations")]
pub struct Model {
    /// Random UUID; the `destinationId` the notifier sees.
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// Notifier plugin instance that sends to this destination.
    pub plugin_instance_id: String,
    /// `channel` or `personal`.
    pub kind: String,
    /// Patron who owns a personal destination; `None` for a channel.
    pub user_id: Option<i64>,
    /// Name shown to administrators or the patron; never a secret.
    pub label: String,
    /// Canonical address document of a personal destination; empty for a
    /// channel. May hold secrets, so it is never logged.
    pub address_json: String,
    /// Permanent send failures since the last delivery.
    pub failure_count: i64,
    /// RFC 3339 time the destination was paused; `None` while active.
    pub paused_at: Option<String>,
    /// Why it was paused (the last failure reason).
    pub paused_reason: Option<String>,
    /// RFC 3339 creation time.
    pub created_at: String,
    /// RFC 3339 time of the last write.
    pub updated_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
