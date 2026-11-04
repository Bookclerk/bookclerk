//! SeaORM entity for the `expressions` table (text primary key).

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "expressions")]
pub struct Model {
    /// Expression id (a random UUID).
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// The work it realises.
    pub work_id: String,
    /// How it realises the work: `narration`, `text`, `translation`, `dramatisation` or `podcast_audio`.
    pub kind: String,
    /// Lower-cased language code; `None` when unknown.
    pub language: Option<String>,
    /// `unknown`, `unabridged` or `abridged`.
    pub abridgement: String,
    /// Sorted, cleaned names of its narrator, translator, host and cast credits.
    pub credit_key: String,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
