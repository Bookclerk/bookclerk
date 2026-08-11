//! SeaORM entity for the `operator_sessions` table.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "operator_sessions")]
pub struct Model {
    /// Surrogate primary key assigned by the database.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// SHA-256 hex digest of the opaque token (plaintext never stored).
    #[sea_orm(unique)]
    pub token_hash: String,
    /// RFC 3339 expiry for the ticket, session, or code.
    pub expires_at: String,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
    /// RFC 3339 time of the last authenticated use of this session.
    pub last_used_at: Option<String>,
    /// User id that elevated into this operator session, if any.
    pub elevated_from_user_id: Option<i64>,
    /// User id being impersonated by this operator session, if any.
    pub impersonating_user_id: Option<i64>,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
