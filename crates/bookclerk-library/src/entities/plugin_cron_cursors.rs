//! SeaORM entity for `plugin_cron_cursors`: the last claimed fire of each
//! plugin cron schedule.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "plugin_cron_cursors")]
pub struct Model {
    /// Plugin instance whose schedule this is.
    #[sea_orm(primary_key, auto_increment = false)]
    pub plugin_instance_id: String,
    /// Normalized cron source (`*/30 * * * *`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub cron: String,
    /// Last claimed fire time in Unix milliseconds.
    pub last_fire_ms: i64,
    /// Token of the batch that last moved the cursor; empty for a new row.
    pub claim: String,
    /// RFC 3339 time of the last write.
    pub updated_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
