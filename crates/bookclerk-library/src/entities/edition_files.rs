//! SeaORM entity for `edition_files`: every file key an acquire or a host
//! upload wrote, per output destination, and the edition it belongs to
//! (library data model ADR, `docs/adr/library-data-model.md`,
//! section 3). Written by the acquire pipeline
//! ([`crate::LibraryStore::record_edition_file`]) and by
//! `control_plane::uploads`, which refuses an upload a key another edition
//! holds and deletes an aborted upload's copy only while the key is still
//! its own.

use sea_orm::entity::prelude::*;

/// Row shape for one recorded file key.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "edition_files")]
pub struct Model {
    /// Output destination id (`local`, `s3`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub destination: String,
    /// Storage key on that destination.
    #[sea_orm(primary_key, auto_increment = false)]
    pub storage_key: String,
    /// `edition`: files belong to editions.
    pub owner_kind: String,
    /// The edition id.
    pub owner_id: String,
    /// What the file is ([`crate::FileRole`]).
    pub role: String,
    /// 1 for a lossless audio encoding; 0 otherwise or unknown (an integer,
    /// as every flag column is, so each database adapter decodes it alike).
    pub lossless: i64,
    /// Audio bitrate in kilobits per second, when known.
    pub bitrate_kbps: Option<i64>,
    /// Lowercase hex SHA-256 of the content, when known (host uploads).
    pub sha256: Option<String>,
    /// The host upload id that claimed the key last, or
    /// [`crate::store::ACQUIRE_WRITER`] for an acquire run on this node.
    pub writer: String,
    /// RFC 3339 time of the latest claim.
    pub written_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
