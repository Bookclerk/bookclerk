//! SeaORM entity for `player_card_tokens`: the access and refresh tokens of
//! player library cards, by SHA-256 (`store::player_cards`).

use sea_orm::entity::prelude::*;

/// Row shape for one token.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "player_card_tokens")]
pub struct Model {
    /// SHA-256 hex of the token; the token itself is never stored.
    #[sea_orm(primary_key, auto_increment = false)]
    pub token_hash: String,
    /// The card's `portal_sessions.id`.
    pub session_id: i64,
    /// `access` (one hour, sent on every request) or `refresh` (thirty
    /// days, traded for a new pair).
    pub kind: String,
    /// RFC 3339 time it stops working.
    pub expires_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
