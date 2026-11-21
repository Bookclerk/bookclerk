//! SeaORM entity for `player_passwords`: passwords a patron made for one
//! player app each (`store::player_cards`). Only a SHA-256 of each is kept.

use sea_orm::entity::prelude::*;

/// Row shape for one player password.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "player_passwords")]
pub struct Model {
    /// Password id (hyphenated UUID).
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// The patron it signs in as (`users.id`).
    pub user_id: i64,
    /// The name the patron gave it, such as the app or phone it is for.
    pub label: String,
    /// SHA-256 hex of the password; the password itself is never stored.
    #[sea_orm(unique)]
    pub password_hash: String,
    /// RFC 3339 time it was made.
    pub created_at: String,
    /// RFC 3339 time it last signed an app in; null before the first time.
    pub last_used_at: Option<String>,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
