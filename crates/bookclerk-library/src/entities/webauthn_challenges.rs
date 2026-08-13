//! SeaORM entity for in-flight WebAuthn ceremonies.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "webauthn_challenges")]
pub struct Model {
    /// Surrogate primary key assigned by the database.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// Public ceremony id returned to the browser.
    #[sea_orm(unique)]
    pub challenge_id: String,
    /// User id for register/elevate; optional for login.
    pub user_id: Option<i64>,
    /// `register`, `login`, or `elevate`.
    pub kind: String,
    /// Serialized webauthn-rs registration or authentication state.
    pub state_json: String,
    /// RFC 3339 expiry.
    pub expires_at: String,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
