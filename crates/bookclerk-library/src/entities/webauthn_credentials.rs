//! SeaORM entity for WebAuthn / passkey credentials.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "webauthn_credentials")]
pub struct Model {
    /// Surrogate primary key assigned by the database.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// First-party user this passkey belongs to.
    pub user_id: i64,
    /// Base64url credential id.
    #[sea_orm(unique)]
    pub credential_id: String,
    /// Serialized `webauthn_rs::prelude::Passkey`.
    pub passkey_json: String,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
    /// RFC 3339 last successful assertion, when known.
    pub last_used_at: Option<String>,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
