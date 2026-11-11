//! SeaORM entity for `account_scan_state`: where each account's storefront
//! scan stands between passes and between pages.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "account_scan_state")]
pub struct Model {
    /// Provenance-qualified PluginKey that owns the account.
    #[sea_orm(primary_key, auto_increment = false)]
    pub plugin_key: String,
    /// Store account id, unique within one plugin key.
    #[sea_orm(primary_key, auto_increment = false)]
    pub account_id: String,
    /// Opaque token the plugin returned at the end of the last pass; empty
    /// asks the next pass to be full.
    pub sync_token: String,
    /// Cursor of the next page of a pass that stopped part way; empty when no
    /// pass is in progress.
    pub resume_cursor: String,
    /// Sync token the in-progress pass started with (sent again on resume).
    pub resume_token: String,
    /// RFC 3339 time the last complete (full) pass ended; `None` before one.
    pub last_complete_at: Option<String>,
    /// Id of the pass in progress or last run; each written page stamps it
    /// on the holdings it listed (`holdings.seen_pass`). Empty before the
    /// first pass.
    pub pass_id: String,
    /// RFC 3339 time of the last write (a resume cursor's age).
    pub updated_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
