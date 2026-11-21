//! SeaORM entity for `ingest_batches`: one batch of uploaded files or
//! intake references, held by one patron. Written by
//! `control_plane::ingest`.

use sea_orm::entity::prelude::*;

/// Row shape for one batch.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "ingest_batches")]
pub struct Model {
    /// Batch id: 32 lower-case hex digits (`control_plane::IngestBatchId`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// The patron who holds every book the batch stores (`users.id`).
    pub user_id: i64,
    /// The node whose disk holds the staged bytes, the only one that runs
    /// the batch.
    pub node_id: String,
    /// `upload` (staged on `node_id`) or `intake` (read from a storage
    /// instance in place).
    pub source: String,
    /// For an intake batch, the storage plugin instance its members are
    /// read from; `None` for an upload.
    pub intake_instance: Option<String>,
    /// `open`, `committed`, `running`, `done`, `failed` or `cancelled`.
    pub state: String,
    /// Members named by the manifest (expanded archive entries excluded).
    pub file_count: i64,
    /// Sum of the manifest's declared sizes, in bytes.
    pub declared_bytes: i64,
    /// The `ingest_batch` job once committed.
    pub job_id: Option<String>,
    /// Why a failed batch failed, for the person who uploaded it.
    pub detail: Option<String>,
    /// RFC 3339 creation time.
    pub created_at: String,
    /// RFC 3339 time of the last activity (a chunk, a commit, a state
    /// change); the idle prune reads it.
    pub updated_at: String,
    /// RFC 3339 time the batch reached a final state.
    pub finished_at: Option<String>,
}

/// Declared SeaORM relations (none).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
