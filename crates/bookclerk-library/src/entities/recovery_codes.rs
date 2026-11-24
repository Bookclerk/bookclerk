//! SeaORM entity for `recovery_codes`: one unused second-factor recovery
//! code of a user, its secret stored as an Argon2id hash.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "recovery_codes")]
pub struct Model {
    /// The user the code recovers.
    #[sea_orm(primary_key, auto_increment = false)]
    pub user_id: i64,
    /// The code's first four characters, as shown; unique per user.
    #[sea_orm(primary_key, auto_increment = false)]
    pub selector: String,
    /// Argon2id PHC hash of the code's other twelve characters.
    pub code_hash: String,
    /// RFC 3339 time the set it belongs to was made.
    pub created_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
