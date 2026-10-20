//! SeaORM entity for `node_upload_sessions`: one title's streamed upload
//! from a host node to an owner node (H-4 design, section 6). Written by
//! `control_plane::uploads`.

use sea_orm::entity::prelude::*;

/// Row shape for one upload.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "node_upload_sessions")]
pub struct Model {
    /// Upload id (hyphenated UUID).
    #[sea_orm(primary_key, auto_increment = false)]
    pub upload_id: String,
    /// `hash_token` of the bearer token; the token itself is never stored.
    pub token_hash: String,
    /// Host node the upload was granted to.
    pub node_id: String,
    /// Acquire job the upload belongs to.
    pub job_id: String,
    /// Lease generation the grant was made under; the upload is accepted
    /// only while it is the job's live fence for this node.
    pub lease_generation: i64,
    /// Owner node that streams the files to its destinations.
    pub receiving_node_id: String,
    /// Library id of the title.
    pub book_uuid: String,
    /// Storage key of the title's primary audio file.
    pub primary_key: String,
    /// The declared files and PDF key (JSON object `{files, pdfKey}`, each
    /// file `{key, size, sha256, content_type}`).
    pub manifest_json: String,
    /// Indexes of the files written to every destination (JSON list).
    pub received_json: String,
    /// `open`, `committed` or `aborted`.
    pub state: String,
    /// RFC 3339 grant time.
    pub created_at: String,
    /// RFC 3339 time after which the token is refused.
    pub expires_at: String,
    /// RFC 3339 commit time.
    pub committed_at: Option<String>,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
