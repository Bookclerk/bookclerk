//! SeaORM entity for `library_index_builds`: finished page index builds.
//!
//! A layout with no row has never been built on this library, so the
//! heartbeat queues a rebuild of `library_work_rows`.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "library_index_builds")]
pub struct Model {
    /// The index layout the build wrote
    /// ([`crate::LIBRARY_INDEX_LAYOUT`]).
    #[sea_orm(primary_key, auto_increment = false)]
    pub layout: i64,
    /// RFC 3339 timestamp when the build finished.
    pub finished_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
