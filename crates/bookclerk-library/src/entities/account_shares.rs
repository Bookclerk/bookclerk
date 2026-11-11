//! SeaORM entity for the `account_shares` table: where a store account
//! travelled with its person (`control_plane::carried_accounts`).
use sea_orm::entity::prelude::*;

/// Row shape for one place an account was sent to or received from.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "account_shares")]
pub struct Model {
    /// Provenance-qualified PluginKey that owns the account.
    #[sea_orm(primary_key, auto_increment = false)]
    pub plugin_key: String,
    /// Store account id, unique within its plugin key.
    #[sea_orm(primary_key, auto_increment = false)]
    pub account_id: String,
    /// The other instance: `node:<node_id>` for a branch this library sent
    /// the account to, `link:<cluster_id>` for a library it came from.
    #[sea_orm(primary_key, auto_increment = false)]
    pub peer: String,
    /// `sent` or `received`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub direction: String,
    /// RFC 3339 time the account last travelled this way.
    pub shared_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
