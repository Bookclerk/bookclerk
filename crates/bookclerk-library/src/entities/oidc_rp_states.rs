//! SeaORM entity for OIDC relying-party authorize state (PKCE + nonce).

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "oidc_rp_states")]
pub struct Model {
    /// Surrogate primary key assigned by the database.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// SHA-256 hex digest of the OAuth `state` parameter.
    #[sea_orm(unique)]
    pub state_hash: String,
    /// Configured provider id (`corp`, `google`, …).
    pub provider_id: String,
    /// PKCE code_verifier (plaintext; short-lived row).
    pub pkce_verifier: String,
    /// OIDC nonce bound to the id_token.
    pub nonce: String,
    /// `login` or `elevate`.
    pub purpose: String,
    /// Owner user id when `purpose = elevate`.
    pub user_id: Option<i64>,
    /// RFC 3339 expiry.
    pub expires_at: String,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
