//! SeaORM entity for `clerk_links`: on a clerk, the libraries this install
//! holds a library card for (written by `control_plane::record_clerk_link`).

use sea_orm::entity::prelude::*;

/// One library this install signed in to with a library card.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "clerk_links")]
pub struct Model {
    /// The library's cluster id, as its token answer named it.
    #[sea_orm(primary_key, auto_increment = false)]
    pub library_cluster_id: String,
    /// The library's canonical origin (`https://library.example`).
    pub origin: String,
    /// SPKI pin confirmed for a self-signed library, when it is one.
    pub spki_pin: Option<String>,
    /// The library's name, as its info reported it at sign-in.
    pub library_name: String,
    /// The person's user id on the library.
    pub remote_user_id: i64,
    /// The person's display name on the library, when it has one.
    pub display_name: Option<String>,
    /// The name this install signed in as (shown in the person's session
    /// list on the library).
    pub clerk_name: String,
    /// `active` while the card is held, `signed_out` once the library
    /// ended it.
    pub state: String,
    /// RFC 3339 time of the sign-in.
    pub created_at: String,
    /// RFC 3339 time of the last change.
    pub updated_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
