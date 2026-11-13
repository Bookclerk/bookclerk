//! SeaORM entity for `edition_acquisitions`: each edition's acquire state.
//!
//! Files belong to editions, so one acquire serves every library title
//! (`books` row) of the edition. Written beside the title's own status by
//! [`crate::LibraryStore::set_acquire_status`]; an edition with no row is
//! not acquired.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "edition_acquisitions")]
pub struct Model {
    /// The edition (`editions.id`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub edition_id: String,
    /// Acquire state (`AcquireStatus::as_str`): `not_acquired`, `queued`,
    /// `downloading`, `acquired` or `error`.
    pub status: String,
    /// Primary destination key of the edition's audio once acquired.
    pub storage_key: Option<String>,
    /// Last acquire failure message for operators.
    pub error_message: Option<String>,
    /// The library title (`books.uuid`) whose write set this row; while it
    /// stays in the library, only its writes change an acquired edition.
    pub title_uuid: String,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
