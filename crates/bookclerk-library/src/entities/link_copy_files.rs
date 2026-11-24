//! SeaORM entity for `link_copy_files`: on a branch, the files a host job
//! copied to this install's own destinations for a library it joined
//! (written by `control_plane::link_copies`).

use sea_orm::entity::prelude::*;

/// One copied file on one of this install's destinations.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "link_copy_files")]
pub struct Model {
    /// This install's destination id (`local`, `s3`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub destination: String,
    /// The file's storage key on that destination.
    #[sea_orm(primary_key, auto_increment = false)]
    pub storage_key: String,
    /// The library the title was acquired for.
    pub library_id: String,
    /// The title the file belongs to: its call number on that library (its
    /// edition), else its holding id there.
    pub title_ref: String,
    /// The copy attempt that wrote it last: `<node id>/<process
    /// incarnation>/<attempt id>`.
    pub writer: String,
    /// `copying` while the attempt runs, `kept` once the library committed
    /// the title.
    pub state: String,
    /// 1 when the attempt took over a kept copy of the same title, so a
    /// failed attempt leaves the file and the record kept.
    pub replaced: i64,
    /// RFC 3339 time of the latest write.
    pub written_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
