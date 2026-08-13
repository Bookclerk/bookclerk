//! SeaORM entity for durable `dbAtomic` result receipts.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "db_atomic_receipts")]
pub struct Model {
    /// Caller-chosen idempotency key (`operationId`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub operation_id: String,
    /// Named operation tag (`deleteUser`, `takeOidcRpState`, …).
    pub operation_kind: String,
    /// SHA-256 hex digest of the canonical `operation` JSON.
    pub request_hash: String,
    /// Application status stored with the first commit.
    pub status: String,
    /// JSON payload when the first commit produced one.
    pub payload: Option<String>,
    /// RFC 3339 timestamp when the receipt was first written.
    pub created_at: String,
    /// RFC 3339 expiry; bounded cleanup deletes older rows.
    pub expires_at: String,
    /// Unique consume-once token (`oidc:<hash>`, `webauthn:<id>:<kind>`).
    pub consume_key: Option<String>,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
