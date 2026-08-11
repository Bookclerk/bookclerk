//! SeaORM entity for the `user_invites` table entity — admin/operator invite tickets with role.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "user_invites")]
pub struct Model {
    /// Surrogate primary key assigned by the database.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// SHA-256 hex digest of the opaque token (plaintext never stored).
    #[sea_orm(unique)]
    pub token_hash: String,
    /// First-party role (`administrator` or `member`).
    pub role: String,
    /// Local username for password login, when set.
    pub login_name: Option<String>,
    /// Human-readable name shown in the UI.
    pub display_name: Option<String>,
    /// RFC 3339 expiry for the ticket, session, or code.
    pub expires_at: String,
    /// RFC 3339 time when the ticket/invite was redeemed, if any.
    pub redeemed_at: Option<String>,
    /// Actor that created the row (user id or operator label).
    pub created_by: String,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
