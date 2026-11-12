//! SeaORM entity for the `member_account_rules` table: the operator's
//! per-member rule "may connect store accounts here".
use sea_orm::entity::prelude::*;

/// Row shape for one member's rule. A member with no row may connect.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "member_account_rules")]
pub struct Model {
    /// The member (`users.id`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub user_id: i64,
    /// `1` when the member may connect store accounts on this instance,
    /// `0` when the operator refused it.
    pub may_connect: i64,
    /// RFC 3339 time the rule was last set.
    pub updated_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
