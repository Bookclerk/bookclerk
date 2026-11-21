//! SeaORM entity for `chapter_list_entries`: one chapter of a stored
//! chapter list (`chapter_lists`), in pre-order.

use sea_orm::entity::prelude::*;

/// Row shape for one chapter of a stored list.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "chapter_list_entries")]
pub struct Model {
    /// The list's digest (`chapter_lists.digest`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub digest: String,
    /// The chapter's place in the list, from 0.
    #[sea_orm(primary_key, auto_increment = false)]
    pub position: i64,
    /// Start, in milliseconds from the start of the edition's timeline.
    pub start_ms: i64,
    /// Nesting depth; 0 for a top-level chapter.
    pub depth: i64,
    /// Chapter title; never empty.
    pub title: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
