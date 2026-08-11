//! SeaORM entity for the `embeddings` table.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "embeddings")]
pub struct Model {
    /// Surrogate primary key assigned by the database.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// Embedding target kind (`book`, `work`, …).
    pub target_kind: String,
    /// Id of the embedded target within `target_kind`.
    pub target_id: String,
    /// Embedding model identifier used to produce `vector`.
    pub model: String,
    /// Dimensionality of the stored embedding vector.
    pub dims: i64,
    /// Raw embedding bytes (little-endian f32 sequence).
    pub vector: Vec<u8>,
    /// Hash of the text that was embedded (skip re-embed when unchanged).
    pub text_hash: String,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
