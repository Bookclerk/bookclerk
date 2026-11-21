//! SeaORM entity for `destination_objects`: the last known state of every
//! object at each output destination (storage destinations ADR,
//! `docs/adr/storage-destinations.md`, decision 5).
//!
//! Written by the destination sync pass from the destination's change feed
//! and its scheduled listing ([`crate::LibraryStore::record_seen_objects`],
//! [`crate::LibraryStore::remove_seen_objects`],
//! [`crate::LibraryStore::finish_listing`]); read by the readers that
//! act on outside changes, which page the rows changed since a sequence
//! number ([`crate::LibraryStore::changed_objects`]).

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "destination_objects")]
pub struct Model {
    /// The destination's plugin instance id.
    #[sea_orm(primary_key, auto_increment = false)]
    pub destination: String,
    /// The object's key at the destination, as its listing names it.
    #[sea_orm(primary_key, auto_increment = false)]
    pub object_key: String,
    /// Object size in bytes when last seen.
    pub size: i64,
    /// The backend's opaque change signature (or entity tag) when last
    /// seen; `None` when the backend gave neither.
    pub signature: Option<String>,
    /// Last modification time in UTC unix milliseconds, when known.
    pub modified_at_ms: Option<i64>,
    /// The destination's change sequence of the last listing or feed page
    /// that saw the object.
    pub listed_seq: i64,
    /// The destination's change sequence when the row last changed (added,
    /// modified or removed).
    pub changed_seq: i64,
    /// 1 once the object is gone (a tombstone kept for readers that page
    /// changes), else 0.
    pub removed: i64,
    /// RFC 3339 time the row last changed.
    pub changed_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
