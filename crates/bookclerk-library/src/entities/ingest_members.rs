//! SeaORM entity for `ingest_members`: one file of an ingest batch and
//! what became of it. Written by `control_plane::ingest`.

use sea_orm::entity::prelude::*;

/// Row shape for one member.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "ingest_members")]
pub struct Model {
    /// The batch (`ingest_batches.id`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub batch_id: String,
    /// Position in the batch, from 0: manifest files first, then the
    /// entries archives expanded into.
    #[sea_orm(primary_key, auto_increment = false)]
    pub member_no: i64,
    /// The archive member this file was expanded from; `None` for a file
    /// the manifest named.
    pub parent_no: Option<i64>,
    /// Relative path in the batch (canonical `bookclerk_archive::EntryPath`
    /// form), or the sanitized name of a refused archive entry.
    pub path: String,
    /// Size in bytes: declared by the manifest, or written by the archive
    /// walk.
    pub size: i64,
    /// Lower-case hex SHA-256 the manifest declared, when it declared one.
    pub sha256: Option<String>,
    /// Bytes staged so far (equal to `size` once complete).
    pub received: i64,
    /// For an intake member, its key in the batch's intake instance.
    pub storage_key: Option<String>,
    /// For an intake member, the storage signature it was listed with.
    pub signature: Option<String>,
    /// `pending`, `expanded`, `matched`, `needs_review`, `duplicate`,
    /// `refused` or `failed`.
    pub outcome: String,
    /// Why it was refused or failed (a stable snake_case reason).
    pub reason: Option<String>,
    /// The edition a stored member belongs to.
    pub edition_id: Option<String>,
    /// RFC 3339 time of the last change.
    pub updated_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
