//! SeaORM entity for `copy_grants`: one reader's leave to download the
//! copy this library holds of one edition. Written by
//! `control_plane::copies`.

use sea_orm::entity::prelude::*;

/// Row shape for one grant.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "copy_grants")]
pub struct Model {
    /// Copy id (hyphenated UUID).
    #[sea_orm(primary_key, auto_increment = false)]
    pub copy_id: String,
    /// `hash_token` of the bearer token; the token itself is never stored.
    pub token_hash: String,
    /// What asked: `node` (a branch) or `card` (a clerk's library card).
    pub holder_kind: String,
    /// The asking node's id, or the card's id.
    pub holder_id: String,
    /// The person the copy is for (`users.id`).
    pub user_id: i64,
    /// The edition copied.
    pub edition_id: String,
    /// The node that answered the grant and serves the files.
    pub serving_node_id: String,
    /// The output destination on that node the files are read from.
    pub destination: String,
    /// Digest of the edition's file rows on that destination at grant, in
    /// download order; a download is refused once they differ.
    pub files_digest: String,
    /// RFC 3339 grant time.
    pub created_at: String,
    /// RFC 3339 time after which the token is refused.
    pub expires_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
