//! SeaORM entity for `edition_files`: every file key an acquire or a host
//! upload wrote, per output destination, and the edition it belongs to
//! (library data model ADR, `docs/adr/library-data-model.md`,
//! section 3). Written by the acquire pipeline
//! ([`crate::LibraryStore::record_edition_file`]) and by
//! `control_plane::uploads`, which refuses an upload a key another edition
//! holds and deletes an aborted upload's copy only while the key is still
//! its own.

use sea_orm::entity::prelude::*;

/// Row shape for one recorded file key.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "edition_files")]
pub struct Model {
    /// Output destination id (`local`, `s3`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub destination: String,
    /// Storage key on that destination.
    #[sea_orm(primary_key, auto_increment = false)]
    pub storage_key: String,
    /// `edition`: files belong to editions.
    pub owner_kind: String,
    /// The edition id.
    pub owner_id: String,
    /// What the file is ([`crate::FileRole`]).
    pub role: String,
    /// 1 for a lossless audio encoding; 0 otherwise or unknown (an integer,
    /// as every flag column is, so each database adapter decodes it alike).
    pub lossless: i64,
    /// Average audio bitrate in kilobits per second, once probed.
    pub bitrate_kbps: Option<i64>,
    /// Media type of the file (`audio/mp4`, `audio/mpeg`, `audio/flac`),
    /// once probed.
    pub content_type: Option<String>,
    /// Plain id of the storefront the copy was downloaded from (`audible`,
    /// `libro`), when the writer knew it.
    pub source_store: Option<String>,
    /// RFC 3339 time the probe result was stored; `None` until the file has
    /// been probed.
    pub probed_at: Option<String>,
    /// Lowercase hex SHA-256 of the content, when known (host uploads).
    pub sha256: Option<String>,
    /// Format of the library copy (`output.copies`) this audio file belongs
    /// to (`m4b`, `mp3`, `mp3_chapters`, `original`); `None` for a file of
    /// no one copy (a sidecar, the PDF) or an upload.
    pub copy_format: Option<String>,
    /// Quality of that copy (`original`, `64k-mono`, `v2`); `None` exactly
    /// when [`Self::copy_format`] is.
    pub copy_quality: Option<String>,
    /// The audio encoding a probe found (`aac`, `mp3`, `opus`, ...), once
    /// probed and when the tag reader knows it.
    pub codec: Option<String>,
    /// The AAC profile (`lc`, `he`, `he_v2`, `xhe`) of an AAC file whose
    /// decoder configuration names one.
    pub aac_profile: Option<String>,
    /// Output channels, when the headers state them.
    pub channels: Option<i64>,
    /// Output sample rate in hertz, when the headers state it.
    pub sample_rate_hz: Option<i64>,
    /// The running time a player sees, in milliseconds; null when unknown,
    /// never zero.
    pub duration_ms: Option<i64>,
    /// The host upload id that claimed the key last, or
    /// [`crate::store::ACQUIRE_WRITER`] for an acquire run on this node.
    pub writer: String,
    /// RFC 3339 time of the latest claim.
    pub written_at: String,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
