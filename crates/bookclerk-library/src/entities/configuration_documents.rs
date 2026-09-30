//! SeaORM entity for one typed configuration document.

use sea_orm::entity::prelude::*;

/// Row shape for a scoped configuration document.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "configuration_documents")]
pub struct Model {
    /// Scope kind (`cluster` or `host`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub scope_type: String,
    /// Scope instance (`singleton` or a host id).
    #[sea_orm(primary_key, auto_increment = false)]
    pub scope_id: String,
    /// Typed domain name (`core.events`, `host.runtime`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub namespace: String,
    /// Domain schema version understood by this binary.
    pub schema_version: i64,
    /// Monotonic compare-and-swap revision. Starts at 1.
    pub revision: i64,
    /// JSON body of the typed domain document.
    pub document_json: String,
    /// RFC 3339 timestamp of the last committed replacement.
    pub updated_at: String,
    /// Audit identity of the last writer.
    pub updated_by: String,
    /// Operation id of the write that produced the current revision.
    pub write_operation_id: String,
}

/// Declared SeaORM relations (documents are addressed by the composite key).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
