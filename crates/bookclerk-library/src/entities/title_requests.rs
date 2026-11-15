//! SeaORM entity for the `title_requests` table.

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "title_requests")]
pub struct Model {
    /// Surrogate primary key assigned by the database.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// Stable UUID for this row (API / foreign-key identity).
    #[sea_orm(unique)]
    pub uuid: String,
    /// Foreign key to `portal_identities.id`.
    pub identity_id: Option<i64>,
    /// The work the wish is for (`works.id`); its title and credits are
    /// the wish's display title and authors.
    pub work_id: String,
    /// The edition the wish named, when the library knows it.
    pub edition_id: Option<String>,
    /// Free-form operator or requester notes.
    pub notes: Option<String>,
    /// Lifecycle status for the row (user, request, …).
    pub status: String,
    /// Preferred storefront plugin id for fulfilling a wishlist item.
    pub preferred_source: Option<String>,
    /// Edition (`editions.id`) the library holds once the wishlist item is fulfilled.
    pub resolved_edition_id: Option<String>,
    /// HTTPS URL for cover art when known.
    pub cover_url: Option<String>,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
