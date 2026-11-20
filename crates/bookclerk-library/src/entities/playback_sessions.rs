//! SeaORM entity for `playback_sessions`: one stretch of listening by one
//! person on one player. A session is also the leave to stream the
//! edition's files from one destination, checked afresh on every request
//! (`control_plane::copies::authorize_playback`).

use sea_orm::entity::prelude::*;

/// Row shape for one play session.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "playback_sessions")]
pub struct Model {
    /// Session id (hyphenated UUID).
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// The listener (`users.id`).
    pub user_id: i64,
    /// The edition (`editions.id`) being played.
    pub edition_id: String,
    /// The node that opened the session and streams its files.
    pub serving_node_id: String,
    /// The output destination the files are read from.
    pub destination: String,
    /// Digest of the edition's file rows on that destination when the
    /// session opened; a stream request is refused once they differ.
    pub files_digest: String,
    /// What kind of player opened it ([`crate::PlayerKind`]).
    pub client_kind: String,
    /// The player's own name for itself, shown in listening history.
    pub client_name: String,
    /// Position the session started from, in milliseconds.
    pub start_position_ms: i64,
    /// Position at the last sync, in milliseconds.
    pub position_ms: i64,
    /// Time spent listening in this session, in milliseconds of wall
    /// clock as the player counted it.
    pub listened_ms: i64,
    /// RFC 3339 time the session opened.
    pub started_at: String,
    /// RFC 3339 time of the last sync.
    pub updated_at: String,
    /// RFC 3339 time the player closed the session; `None` while open.
    pub closed_at: Option<String>,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
