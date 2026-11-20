//! SeaORM entity for `account_downloads`: each download that reached a
//! store, counted against the account's rolling daily budget.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "account_downloads")]
pub struct Model {
    /// Random row id (UUID).
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// Provenance-qualified PluginKey that owns the account.
    pub plugin_key: String,
    /// Store account id, unique within one plugin key.
    pub account_id: String,
    /// When the download started, in Unix milliseconds.
    pub started_at_ms: i64,
    /// The acquire job that ran it, when one did.
    pub job_id: Option<String>,
    /// 1 when the host started the download itself, 0 when a person did.
    pub paced: i64,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
