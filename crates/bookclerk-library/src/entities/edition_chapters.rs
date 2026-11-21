//! SeaORM entity for `edition_chapters`: the chapter list an edition's
//! claims resolved to (`store::chapters`).

use sea_orm::entity::prelude::*;

/// Row shape for one edition's resolved chapters.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "edition_chapters")]
pub struct Model {
    /// The edition (`editions.id`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub edition_id: String,
    /// The winning claim's list (`chapter_lists.digest`).
    pub digest: String,
    /// The winning claim's source kind ([`crate::ChapterSourceKind`]).
    pub source_kind: String,
    /// The winning claim's source id.
    pub source_id: String,
    /// Length of the timeline the list was measured on, in milliseconds.
    pub runtime_ms: Option<i64>,
    /// The highest claim revision the resolution read.
    pub revision: i64,
    /// RFC 3339 time of the last change.
    pub updated_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
