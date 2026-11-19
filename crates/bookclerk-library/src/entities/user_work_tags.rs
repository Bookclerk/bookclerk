//! SeaORM entity for `user_work_tags`: one user's tags on a work, one row
//! per tag.
//!
//! Written with `user_title_state.tags` by
//! [`crate::LibraryStore::set_user_title_state`], so the library page can
//! filter by a tag.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "user_work_tags")]
pub struct Model {
    /// The user (`users.id`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub user_id: i64,
    /// The tag lower-cased, which a filter matches.
    #[sea_orm(primary_key, auto_increment = false)]
    pub tag_key: String,
    /// The work (`works.id`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub work_id: String,
    /// The tag as the user wrote it.
    pub tag: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
