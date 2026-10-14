//! SeaORM entity for `plugin_release_pointers`: the active release per plugin.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "plugin_release_pointers")]
pub struct Model {
    /// Canonical plugin key.
    #[sea_orm(primary_key, auto_increment = false)]
    pub plugin_key: String,
    /// Active release version.
    pub version: String,
    /// Compare-and-swap revision of the pointer.
    pub revision: i64,
    /// RFC 3339 time of the last move.
    pub updated_at: String,
    /// Audit identity of the last writer.
    pub updated_by: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
