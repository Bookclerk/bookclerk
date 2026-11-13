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
    /// The revision that crossed this share: on a library, the newest one
    /// it applied from its keeper; on a keeper, the newest one it sent.
    /// Each push a keeper sends carries a higher one than the last.
    pub revision: i64,
    /// On a keeper's `link:` row, the fingerprint of the sign-in it last
    /// pushed ([`crate::control_plane::SignIn::fingerprint`]); a different
    /// one means a new sign-in to send. 0 on a library's rows.
    pub observed: i64,
    /// RFC 3339 time this row was last written.
    pub updated_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
