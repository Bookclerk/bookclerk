//! SeaORM entity for `library_work_rows`: the library's page index.
//!
//! One row per held top-level work, refreshed from the library model by
//! [`crate::LibraryStore::refresh_library_works`], so list views page one
//! table instead of joining works, editions, listings and holdings.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "library_work_rows")]
pub struct Model {
    /// The work (`works.id`); a top-level work, never a podcast episode.
    #[sea_orm(primary_key, auto_increment = false)]
    pub work_id: String,
    /// Work kind (`WorkKind::as_str`): `book` or `podcast`.
    pub kind: String,
    /// The work's display title.
    pub title: String,
    /// Sort key of the title: lower case, a leading article dropped.
    pub sort_title: String,
    /// Authors (or a show's hosts) joined for display, in billing order.
    pub authors: Option<String>,
    /// Sort key of the first author; empty when the work names none.
    pub sort_author: String,
    /// Name of the first series the work belongs to.
    pub series: Option<String>,
    /// The work's position in [`Self::series`], as the store wrote it.
    pub series_position: Option<String>,
    /// The edition whose cover the list shows (`editions.id`).
    pub cover_edition_id: Option<String>,
    /// Sorted, comma-joined media kinds of the work's editions
    /// (`MediaKind::as_str`), such as `audio,ebook`.
    pub media_kinds: String,
    /// The furthest acquire state any edition of the work reached
    /// (`AcquireStatus::as_str`).
    pub acquire_status: String,
    /// How many editions the work has.
    pub edition_count: i64,
    /// How many holdings (account or node) the work's listings have.
    pub holding_count: i64,
    /// RFC 3339 timestamp of the work's newest holding, for "recently
    /// added".
    pub added_at: String,
    /// RFC 3339 timestamp at which the refresh that wrote this row read the
    /// model; a refresh that read earlier never overwrites it.
    pub refreshed_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
