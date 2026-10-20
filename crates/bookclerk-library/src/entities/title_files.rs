//! SeaORM entity for `title_files`: every file key a title's acquire or
//! host upload wrote, per output destination (H-4 design, section 6).
//! Written by the acquire pipeline ([`crate::LibraryStore::record_title_file`])
//! and by `control_plane::uploads`, which refuses an upload a key another
//! title holds and deletes an aborted upload's copy only while the key is
//! still its own.

use sea_orm::entity::prelude::*;

/// Row shape for one recorded file key.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "title_files")]
pub struct Model {
    /// Output destination id (`local`, `s3`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub destination: String,
    /// Storage key on that destination.
    #[sea_orm(primary_key, auto_increment = false)]
    pub storage_key: String,
    /// Library id (`books.uuid`) of the title the file belongs to.
    pub book_uuid: String,
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
