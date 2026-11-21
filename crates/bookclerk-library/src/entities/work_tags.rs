//! SeaORM entity for `work_tags`: library tags on a top-level work, one
//! row per tag and source.
//!
//! An Administrator's tags are written by
//! [`crate::LibraryStore::set_work_tags`]; the page index refresh copies
//! every tag of a work into `library_work_facets` as facet `tag`, which the
//! Library filter and patron scopes match.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "work_tags")]
pub struct Model {
    /// The top-level work (`works.id`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub work_id: String,
    /// Where the tag came from: `admin` (set in Bookclerk) or `sidecar`
    /// (read from a metadata file beside the book).
    #[sea_orm(primary_key, auto_increment = false)]
    pub source: String,
    /// The tag trimmed and lower-cased, which a filter or scope matches.
    #[sea_orm(primary_key, auto_increment = false)]
    pub key: String,
    /// The tag as it was written.
    pub value: String,
    /// When the tag was added (RFC 3339).
    pub created_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
