//! SeaORM entity for live per-node `event_subscriber_nodes` registrations.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "event_subscriber_nodes")]
pub struct Model {
    /// Daemon node id (primary key part; files-dir UUID).
    #[sea_orm(primary_key, auto_increment = false)]
    pub node_id: String,
    /// Subscriber plugin id (primary key part).
    #[sea_orm(primary_key, auto_increment = false)]
    pub plugin_id: String,
    /// JSON array of catalog subscriptions (`type`, `schema_versions`, …).
    pub subscriptions_json: String,
    /// `1` when this node wants matching deliveries for `plugin_id`.
    pub enabled: i64,
    /// RFC 3339 last heartbeat from this node.
    pub heartbeat_at: String,
}

/// Declared SeaORM relations (catalog rows are looked up by node + plugin).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
