//! SeaORM entity for `library_work_facets`: the page index's filter values.
//!
//! One row per value a held top-level work has under a facet (author,
//! narrator, series, genre, publisher or source), written with the work's
//! `library_work_rows` row by
//! [`crate::LibraryStore::refresh_library_works`].

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "library_work_facets")]
pub struct Model {
    /// The work (`works.id`); a top-level work, never a podcast episode.
    #[sea_orm(primary_key, auto_increment = false)]
    pub work_id: String,
    /// The facet (`WorkFacet::as_str`), such as `genre`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub facet: String,
    /// The value lower-cased, which a filter matches.
    #[sea_orm(primary_key, auto_increment = false)]
    pub key: String,
    /// The value as the library shows it.
    pub value: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
