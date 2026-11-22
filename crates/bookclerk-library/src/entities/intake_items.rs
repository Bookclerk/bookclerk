//! SeaORM entity for `intake_items`: one item seen in a drop folder and how
//! far its ingest has come.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "intake_items")]
pub struct Model {
    /// The drop folder: a storage plugin instance with the intake role.
    #[sea_orm(primary_key, auto_increment = false)]
    pub plugin_instance_id: String,
    /// Lowercase hex SHA-256 of `item_key`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub item_hash: String,
    /// The item's path relative to the folder root: its top-level name, or
    /// `<patron folder>/<name>` in subfolder mode.
    pub item_key: String,
    /// `settling`, `queued`, `done` or `failed`.
    pub state: String,
    /// The item's signature as 64 hex characters.
    pub signature: String,
    /// Unix milliseconds since which `signature` has not changed.
    pub quiet_since_ms: i64,
    /// The ingest batch the item was handed to, once queued.
    pub batch_id: Option<String>,
    /// Why the last ingest failed, bounded; `None` otherwise.
    pub error: Option<String>,
    /// RFC 3339 time of the last write.
    pub updated_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
