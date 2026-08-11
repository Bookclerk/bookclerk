//! SeaORM entity for the `oidc_refresh_tokens` — refresh tokens bound to a User.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "oidc_refresh_tokens")]
pub struct Model {
    /// Surrogate primary key assigned by the database.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// SHA-256 hex digest of the opaque token (plaintext never stored).
    #[sea_orm(unique)]
    pub token_hash: String,
    /// Registered OIDC client_id.
    pub client_id: String,
    /// Linked first-party [`users::Model`](super::users::Model) row id, when claimed.
    pub user_id: i64,
    /// OAuth scope string granted to the client.
    pub scope: String,
    /// RFC 3339 expiry for the ticket, session, or code.
    pub expires_at: String,
    /// RFC 3339 time when the refresh token was revoked, if any.
    pub revoked_at: Option<String>,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
