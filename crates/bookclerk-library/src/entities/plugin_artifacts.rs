//! SeaORM entity for `plugin_artifacts`: small version archives stored
//! in the library when no shared artifact location exists.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "plugin_artifacts")]
pub struct Model {
    /// Lowercase hex SHA-256 of the archive bytes.
    #[sea_orm(primary_key, auto_increment = false)]
    pub sha256: String,
    /// Archive bytes as base64 text.
    pub data: String,
    /// Archive size in bytes.
    pub size: i64,
    /// RFC 3339 insert time.
    pub created_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
