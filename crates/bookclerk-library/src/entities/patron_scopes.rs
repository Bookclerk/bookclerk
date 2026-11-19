//! SeaORM entity for the `patron_scopes` table: a Member's own patron
//! scope, set by an Administrator.
use sea_orm::entity::prelude::*;

/// Row shape for one Member's scope. A Member with no row follows the
/// library's default scope.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "patron_scopes")]
pub struct Model {
    /// The Member (`users.id`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub user_id: i64,
    /// The scope as JSON (`crate::PatronScope`, schema version 1).
    pub scope_json: String,
    /// RFC 3339 time the scope was last set.
    pub updated_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
