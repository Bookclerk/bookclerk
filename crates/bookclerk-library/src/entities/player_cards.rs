//! SeaORM entity for `player_cards`: how a player library card (a `player`
//! row of `portal_sessions`) was signed in to (`store::player_cards`).

use sea_orm::entity::prelude::*;

/// Row shape for one player card.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "player_cards")]
pub struct Model {
    /// The card's `portal_sessions.id`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub session_id: i64,
    /// The player password it was made with; null for a card made through
    /// the library's own sign-in page.
    pub password_id: Option<String>,
    /// RFC 3339 time it was made.
    pub created_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
