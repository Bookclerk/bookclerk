//! SeaORM entity for `plugin_webhooks`: one inbound webhook of one plugin
//! instance, with its URL token (storage destinations ADR,
//! `docs/adr/storage-destinations.md`, decision 6).

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "plugin_webhooks")]
pub struct Model {
    /// Owning plugin instance id.
    #[sea_orm(primary_key, auto_increment = false)]
    pub instance_id: String,
    /// `[[webhooks]]` name.
    #[sea_orm(primary_key, auto_increment = false)]
    pub name: String,
    /// Lowercase hex SHA-256 of the URL token; the public route looks the
    /// webhook up by it.
    pub token_hash: String,
    /// The URL token sealed with the library key, base64.
    pub token_sealed: String,
    /// Seal nonce, base64.
    pub token_nonce: String,
    /// 1 when a request was dropped since the last pull, else 0.
    pub missed: i64,
    /// RFC 3339 time the token was minted.
    pub created_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
