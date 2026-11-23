//! SeaORM entity for the `embeddings` table.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "embeddings")]
pub struct Model {
    /// Surrogate primary key assigned by the database.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// What was embedded: `work` (a `works.id`) or `text` (a text shared
    /// by every candidate that has it).
    pub target_kind: String,
    /// Id of the embedded target within `target_kind`: the work id, or the
    /// SHA-256 hex of the text.
    pub target_id: String,
    /// What made `vector`: `@<plugin id>/<model id>`, or `local-hash-v1`
    /// for the stand-in.
    pub model: String,
    /// The model's digest of everything that decides its output; empty for
    /// the stand-in. Vectors are compared only inside one (`model`,
    /// `revision`).
    pub revision: String,
    /// BCP 47 tags of the languages the model was loaded for, joined by
    /// `,`; empty when it has no language packs.
    pub packs: String,
    /// Dimensionality of the stored embedding vector.
    pub dims: i64,
    /// Raw embedding bytes (little-endian f32 sequence).
    pub vector: Vec<u8>,
    /// Hash of the text that was embedded (skip re-embed when unchanged).
    pub text_hash: String,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
    /// RFC 3339 timestamp when the vector was written or, for a `text`
    /// row, last used.
    pub updated_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
