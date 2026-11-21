//! SeaORM entity for `edition_copies`: each edition's copy at each output
//! destination and where it stands (storage destinations ADR,
//! `docs/adr/storage-destinations.md`, decision 3).
//!
//! Written by the acquire pipeline once each destination's write ends
//! ([`crate::LibraryStore::set_copy_stored`],
//! [`crate::LibraryStore::set_copy_pending`]) and by a host upload's commit
//! on the owner; read by acquire candidate selection, which visits an
//! acquired edition again while a destination that takes it lacks a stored
//! copy.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "edition_copies")]
pub struct Model {
    /// The edition (`editions.id`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub edition_id: String,
    /// The destination's plugin instance id.
    #[sea_orm(primary_key, auto_increment = false)]
    pub destination: String,
    /// Where the copy stands (`CopyState::as_str`): `stored`, `pending`, `missing` or
    /// `failed`.
    pub state: String,
    /// The edition's primary audio key on this destination; set once
    /// stored, and kept from an earlier stored copy while a later write is
    /// retried.
    pub storage_key: Option<String>,
    /// Failed writes since the copy was last stored (0 once stored).
    pub attempts: i64,
    /// RFC 3339 time the next background attempt is due; `None` unless
    /// pending.
    pub next_attempt_at: Option<String>,
    /// The last write failure, for operators; `None` once stored.
    pub last_error: Option<String>,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
