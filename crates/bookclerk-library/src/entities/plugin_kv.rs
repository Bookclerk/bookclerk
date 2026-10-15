//! SeaORM entity for the per-plugin durable key/value table `plugin_kv`.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "plugin_kv")]
pub struct Model {
    /// Owning plugin PluginKey (canonical text).
    #[sea_orm(primary_key, auto_increment = false)]
    pub plugin_key: String,
    /// Namespace a `[[kv_namespaces]]` row names.
    #[sea_orm(primary_key, auto_increment = false)]
    pub namespace: String,
    /// Key within the namespace.
    #[sea_orm(primary_key, auto_increment = false)]
    pub key: String,
    /// Value bytes stored as base64 text (portable across backends).
    pub value: String,
    /// Opaque JSON metadata text; empty when none.
    pub metadata: String,
    /// RFC 3339 expiry; empty when the key never expires.
    pub expires_at: String,
    /// RFC 3339 time of the last write.
    pub updated_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
