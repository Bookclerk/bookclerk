//! SeaORM entity for the `users` table entity — first-party identity principal (#117).

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "users")]
pub struct Model {
    /// Surrogate primary key assigned by the database.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// First-party role (`owner`, `administrator`, or `member`).
    pub role: String,
    /// Lifecycle status for the row (user, request, …).
    pub status: String,
    /// Human-readable name shown in the UI.
    pub display_name: Option<String>,
    /// Local username for password login, when set.
    pub login_name: Option<String>,
    /// Optional contact email for invites / notifications.
    pub email: Option<String>,
    /// Argon2id PHC hash for local login; never plaintext.
    pub password_hash: Option<String>,
    /// Incremented to invalidate existing sessions after security changes.
    pub security_version: i64,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
