//! SeaORM entity for the `oidc_clients` — registered OAuth/OIDC clients (ABS, etc.).

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
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
