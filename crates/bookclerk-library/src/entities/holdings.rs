//! SeaORM entity for the `holdings` table: who holds a store listing.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "holdings")]
pub struct Model {
    /// Random id of this holding; a holding removed and added again gets a
    /// new one, so its `holding_added` event is published again.
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// The listing held.
    pub listing_id: String,
    /// Stored holder key, `account:<account_id>` or `node:<node_id>`
    /// (`bookclerk_resolve::Holder::stored`); unique with `listing_id`.
    pub holder: String,
    /// `account` or `node`: which of the typed holder columns is set.
    pub holder_kind: String,
    /// PluginKey of the holding account (the listing's plugin); `None` for a
    /// node holder.
    pub plugin_key: Option<String>,
    /// Holding store account, with `plugin_key` a foreign key into
    /// `accounts`; `None` for a node holder.
    pub account_id: Option<String>,
    /// Holding branch library node (`nodes.node_id`); `None` for an account
    /// holder.
    pub node_id: Option<String>,
    /// `purchase`, `plus`, `loan` or `free`.
    pub entitlement: String,
    /// RFC 3339 time it was bought or added, when known.
    pub purchased_at: Option<String>,
    /// Id of the scan pass that last listed this holding
    /// (`account_scan_state.pass_id`); `None` until a pass lists it.
    pub seen_pass: Option<String>,
    /// Where the holder's copy came from when it is not this library's own
    /// acquire: `link:<cluster_id>` for a book this branch's owner acquired
    /// for the library it joined, imported here as well. `None` for this
    /// library's own work.
    pub provenance: Option<String>,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
