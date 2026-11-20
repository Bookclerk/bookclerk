//! SeaORM entity for `metadata_claims`: one source's value for one field
//! of one subject (`docs/adr/metadata-claims-and-file-sync.md`). Chapters
//! of an edition are the only field so far (`store::chapters`).

use sea_orm::entity::prelude::*;

/// Row shape for one claim.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "metadata_claims")]
pub struct Model {
    /// What the claim is about: `edition`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub subject_kind: String,
    /// The subject's id (`editions.id`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub subject_id: String,
    /// The field: `chapters`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub field: String,
    /// The kind of source ([`crate::ChapterSourceKind`] spelling).
    #[sea_orm(primary_key, auto_increment = false)]
    pub source_kind: String,
    /// Which source of that kind: a plugin key for a provider or a store,
    /// empty for a person's edit and for Bookclerk's own processing.
    #[sea_orm(primary_key, auto_increment = false)]
    pub source_id: String,
    /// BCP 47 language of the value; empty when unknown or not applicable.
    #[sea_orm(primary_key, auto_increment = false)]
    pub locale: String,
    /// Where a machine source ranks ([`crate::ClaimQuality`] spelling);
    /// empty for people and for Bookclerk's own processing.
    pub quality: String,
    /// Digest of the value (a `chapter_lists` row for chapters).
    pub value_digest: String,
    /// Length of the timeline the value was measured on, in milliseconds.
    pub runtime_ms: Option<i64>,
    /// For a processed claim, the digest of the value it was derived from.
    pub input_digest: Option<String>,
    /// The subject's claim counter when this claim last changed.
    pub revision: i64,
    /// RFC 3339 time an Administrator pinned the claim; `None` when not
    /// pinned.
    pub pinned_at: Option<String>,
    /// The Administrator who pinned it.
    pub pinned_by_user_id: Option<i64>,
    /// The person whose edit the claim is, for a person's edit.
    pub created_by_user_id: Option<i64>,
    /// RFC 3339 time the claim last changed.
    pub updated_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
