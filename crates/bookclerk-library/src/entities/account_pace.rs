//! SeaORM entity for `account_pace`: the operator's override of a
//! storefront's download pace for one store account, and the store's
//! cool-down.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "account_pace")]
pub struct Model {
    /// Provenance-qualified PluginKey that owns the account.
    #[sea_orm(primary_key, auto_increment = false)]
    pub plugin_key: String,
    /// Store account id, unique within one plugin key.
    #[sea_orm(primary_key, auto_increment = false)]
    pub account_id: String,
    /// `default`, `custom` or `unlimited` ([`crate::pacing::PaceOverride`]).
    pub pace_mode: String,
    /// Custom downloads per rolling 24 hours; `None` keeps the storefront's.
    pub downloads_per_day: Option<i64>,
    /// Custom seconds between downloads; `None` keeps the storefront's.
    pub min_gap_seconds: Option<i64>,
    /// End of the store's cool-down in Unix milliseconds; `None` for none.
    pub cooldown_until_ms: Option<i64>,
    /// RFC 3339 time of the last write.
    pub updated_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
