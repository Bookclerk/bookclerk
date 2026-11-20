//! SeaORM entity for `playback_progress`: how far one person has listened
//! to one edition. Written by [`crate::LibraryStore`]'s playback methods
//! (`store::playback`).

use sea_orm::entity::prelude::*;

/// Row shape for one person's position in one edition.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "playback_progress")]
pub struct Model {
    /// The listener (`users.id`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub user_id: i64,
    /// The edition (`editions.id`) the position is in.
    #[sea_orm(primary_key, auto_increment = false)]
    pub edition_id: String,
    /// Position on the edition's timeline, in milliseconds from its start.
    pub position_ms: i64,
    /// The edition's whole runtime in milliseconds as the last player
    /// reported it; `None` until one has.
    pub duration_ms: Option<i64>,
    /// `1` once the person reached the end of this edition, else `0`.
    pub is_finished: i64,
    /// RFC 3339 time the person first played the edition.
    pub started_at: String,
    /// RFC 3339 time the person last reached the end; `None` until then.
    pub finished_at: Option<String>,
    /// RFC 3339 time of the last write; orders Continue listening.
    pub updated_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
