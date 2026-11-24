//! SeaORM entity for `email_verifications`: the live verification link of
//! one user's email address, stored as a hash.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "email_verifications")]
pub struct Model {
    /// SHA-256 hex digest of the link's token (the token is never stored).
    #[sea_orm(primary_key, auto_increment = false)]
    pub token_hash: String,
    /// The user whose address it verifies; one live link per user.
    #[sea_orm(unique)]
    pub user_id: i64,
    /// The address the link was sent to; it verifies only while the user
    /// still has it.
    pub email: String,
    /// RFC 3339 time the link stops working.
    pub expires_at: String,
    /// RFC 3339 time it was made.
    pub created_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
