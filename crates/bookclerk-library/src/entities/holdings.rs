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
    /// (`account_scan_state.pass_id`, or the branch's catalog pass start for
    /// a node holding); `None` until a pass lists it.
    pub seen_pass: Option<String>,
    /// For a node holding, `store:product_id` as the branch named the title
    /// (`CatalogRef`); `None` for an account holding.
    pub share_ref: Option<String>,
    /// For a node holding, the title as the branch shared it (JSON
    /// `CatalogTitle`: metadata and file manifest), passed on unchanged
    /// when this library is itself a branch.
    pub share_title: Option<String>,
    /// For a node holding, the digest of the shared manifest
    /// (`CatalogTitle::manifest_digest`).
    pub share_digest: Option<String>,
    /// For a node holding, the manifest digest whose files reached this
    /// library; equal to `share_digest` once the current files arrived.
    pub delivered_digest: Option<String>,
    /// For a node holding, the RFC 3339 time its files last arrived.
    pub delivered_at: Option<String>,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
