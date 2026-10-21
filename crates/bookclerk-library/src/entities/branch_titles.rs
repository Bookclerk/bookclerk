//! SeaORM entity for `branch_titles`: on a library, the titles a branch
//! shared with it (`catalog.sync`; written by `control_plane::branch_catalog`).
//! Rows name the branch's node and its owner user only; nothing names the
//! branch's store accounts, users or own branches.

use sea_orm::entity::prelude::*;

/// Row shape for one shared title.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "branch_titles")]
pub struct Model {
    /// Surrogate primary key.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// Stable id (an upload's `book_uuid`).
    #[sea_orm(unique)]
    pub uuid: String,
    /// `node:<node_id>` of the branch that shared it.
    pub provenance: String,
    /// The branch's node id.
    pub node_id: String,
    /// The user who owns the branch's node: the title is theirs here.
    pub owner_user_id: Option<i64>,
    /// Store id (`audible`, `libro`, ...).
    pub store: String,
    /// Store product id.
    pub product_id: String,
    /// Display title.
    pub title: String,
    /// Authors.
    pub authors: Option<String>,
    /// The shared metadata (JSON `CatalogMetadata`).
    pub metadata_json: String,
    /// Storage key of the primary audio file.
    pub primary_key: String,
    /// Storage key of the companion PDF.
    pub pdf_key: Option<String>,
    /// The shared file manifest (JSON list of `CatalogFile`).
    pub manifest_json: String,
    /// Digest of the manifest (`CatalogTitle::manifest_digest`).
    pub manifest_sha256: String,
    /// The manifest digest whose files were delivered, if any.
    pub delivered_sha256: Option<String>,
    /// `shared` or `detached`.
    pub state: String,
    /// Why it was detached: `stopped_sharing`, `left` or `withdrawn`.
    pub detached_reason: Option<String>,
    /// RFC 3339 first share.
    pub created_at: String,
    /// RFC 3339 last change.
    pub updated_at: String,
    /// RFC 3339 time the branch last named it.
    pub seen_at: String,
    /// RFC 3339 time its files were delivered.
    pub delivered_at: Option<String>,
    /// RFC 3339 time it was detached.
    pub detached_at: Option<String>,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
