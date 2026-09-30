//! SeaORM entity for successful configuration commits.

use sea_orm::entity::prelude::*;

/// Row shape for one committed configuration change.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "configuration_audit")]
pub struct Model {
    /// Surrogate primary key.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// Scope kind copied from the document at commit time.
    pub scope_type: String,
    /// Scope instance copied from the document at commit time.
    pub scope_id: String,
    /// Domain namespace copied from the document at commit time.
    pub namespace: String,
    /// Domain schema version after the commit.
    pub schema_version: i64,
    /// Document revision after the commit.
    pub revision: i64,
    /// Audit identity of the writer.
    pub actor: String,
    /// RFC 3339 timestamp of the commit.
    pub recorded_at: String,
}

/// Declared SeaORM relations (audit rows are append-only).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
