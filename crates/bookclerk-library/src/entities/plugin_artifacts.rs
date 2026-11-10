//! SeaORM entity for `plugin_artifacts`: the header of a version archive
//! stored in the library when no shared artifact location exists.
//!
//! The bytes live in `plugin_artifact_chunks`, written before this row, so
//! a header means every chunk is stored.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "plugin_artifacts")]
pub struct Model {
    /// Lowercase hex SHA-256 of the archive bytes.
    #[sea_orm(primary_key, auto_increment = false)]
    pub sha256: String,
    /// Archive size in bytes.
    pub size: i64,
    /// Bytes per chunk in `plugin_artifact_chunks` (the last may be
    /// shorter); picks which chunk set belongs to this archive.
    pub chunk_bytes: i64,
    /// RFC 3339 insert time.
    pub created_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
