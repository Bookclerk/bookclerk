//! SeaORM entity for the `security_audit_events` table entity — elevate / impersonate / login audit.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "security_audit_events")]
pub struct Model {
    /// Surrogate primary key assigned by the database.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// RFC 3339 timestamp of the audit event.
    pub at: String,
    /// Actor label (user id, operator, or system).
    pub actor: String,
    /// Audit action verb (for example `login`, `rotate_token`).
    pub action: String,
    /// JSON object with structured event details (no secrets).
    pub detail_json: Option<String>,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
