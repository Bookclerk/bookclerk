//! SeaORM entity for `chapter_lists`: one stored chapter list, named by
//! its digest. Written by [`crate::LibraryStore`]'s chapter methods
//! (`store::chapters`) after every entry of the list.

use sea_orm::entity::prelude::*;

/// Row shape for one stored chapter list.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "chapter_lists")]
pub struct Model {
    /// Lower-case hex SHA-256 of the list's canonical encoding.
    #[sea_orm(primary_key, auto_increment = false)]
    pub digest: String,
    /// How many entries the list has (`chapter_list_entries` rows).
    pub chapter_count: i64,
    /// RFC 3339 time the list was first stored.
    pub created_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
