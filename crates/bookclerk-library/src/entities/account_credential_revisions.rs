//! SeaORM entity for the `account_credential_revisions` table: the newest
//! sign-in of a single-keeper account that crossed each share
//! (`control_plane::keeper`).
use sea_orm::entity::prelude::*;

/// Row shape for one account's revision on one share.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "account_credential_revisions")]
pub struct Model {
    /// Provenance-qualified PluginKey that owns the account.
    #[sea_orm(primary_key, auto_increment = false)]
    pub plugin_key: String,
    /// Store account id, unique within its plugin key.
    #[sea_orm(primary_key, auto_increment = false)]
    pub account_id: String,
    /// The other instance, as in `account_shares.peer`: `node:<node_id>`
    /// for the keeper branch whose pushes this library applied,
    /// `link:<cluster_id>` for a library this keeper pushed to.
    #[sea_orm(primary_key, auto_increment = false)]
    pub peer: String,
    /// The keeper's revision of the account's sign-in: milliseconds since
    /// the Unix epoch of its newest secret write on the keeper.
    pub revision: i64,
    /// RFC 3339 time this row was last written.
    pub updated_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
