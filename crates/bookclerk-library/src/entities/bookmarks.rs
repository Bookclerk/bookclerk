//! SeaORM entity for `bookmarks`: places one person marked in one
//! edition. Written by [`crate::LibraryStore`]'s playback methods
//! (`store::playback`).

use sea_orm::entity::prelude::*;

/// Row shape for one bookmark.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "bookmarks")]
pub struct Model {
    /// Bookmark id (hyphenated UUID).
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// The person who made it (`users.id`).
    pub user_id: i64,
    /// The edition (`editions.id`) it marks a place in.
    pub edition_id: String,
    /// The marked place, in milliseconds from the edition's start.
    pub position_ms: i64,
    /// The person's note for it; may be empty.
    pub title: String,
    /// RFC 3339 time it was made.
    pub created_at: String,
    /// RFC 3339 time of the last change.
    pub updated_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
