//! SeaORM entity for `catalog_lookups`: on a library, the catalog
//! questions waiting for (or answered by) each branch
//! (`control_plane::catalog_lookup`).

use sea_orm::entity::prelude::*;

/// One question to one branch about one work.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "catalog_lookups")]
pub struct Model {
    /// The branch node asked.
    #[sea_orm(primary_key, auto_increment = false)]
    pub node_id: String,
    /// This library's work the question is about.
    #[sea_orm(primary_key, auto_increment = false)]
    pub work_id: String,
    /// RFC 3339 time the question was (last) asked.
    pub asked_at: String,
    /// RFC 3339 time it last went out to the branch; `None` while unsent.
    pub sent_at: Option<String>,
    /// RFC 3339 time the branch answered; `None` while waiting.
    pub answered_at: Option<String>,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
