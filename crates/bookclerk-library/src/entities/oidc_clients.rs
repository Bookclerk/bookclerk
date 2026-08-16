//! SeaORM entity for the `oidc_clients` — registered OAuth/OIDC clients.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "oidc_clients")]
pub struct Model {
    /// Surrogate primary key assigned by the database.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// Registered OIDC client_id.
    #[sea_orm(unique)]
    pub client_id: String,
    /// Hash of the OIDC client secret (plaintext never stored).
    pub client_secret_hash: Option<String>,
    /// JSON array of allowed OAuth redirect URIs.
    pub redirect_uris_json: String,
    /// Human-readable or logical name for this row.
    pub name: Option<String>,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
    /// When true, token responses include a refresh token.
    pub issue_refresh_token: i64,
    /// JSON array of scopes this client may be granted.
    pub allowed_scopes_json: String,
    /// When non-zero, authorize and token endpoints accept this client.
    pub enabled: i64,
    /// Plugin id that owns this client; `None` for operator-created clients.
    pub plugin_id: Option<String>,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
