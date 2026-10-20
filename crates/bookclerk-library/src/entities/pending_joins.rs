//! SeaORM entity for `pending_joins`: on a host joining a library, one
//! sign-in in progress (written by `control_plane::create_pending_join`).
//! The PKCE verifier and the node keys are sealed in `encrypted_secrets`,
//! never stored here.

use sea_orm::entity::prelude::*;

/// Row shape for one pending join.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "pending_joins")]
pub struct Model {
    /// `hash_token` of the OAuth `state`; the state itself is not stored.
    #[sea_orm(primary_key, auto_increment = false)]
    pub state_hash: String,
    /// Owner origin (`https://owner.example`), canonical.
    pub owner_origin: String,
    /// SPKI pin the operator confirmed (self-signed owners).
    pub spki_pin: Option<String>,
    /// Owner cluster id `GET /api/node/v1/info` reported.
    pub owner_cluster_id: String,
    /// Library name the owner reported.
    pub library_name: String,
    /// Name this install asks to join as.
    pub branch_name: String,
    /// Redirect URI the authorization was requested with.
    pub redirect_uri: String,
    /// RFC 3339 creation time.
    pub created_at: String,
    /// RFC 3339 time after which the row is refused and purged.
    pub expires_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
