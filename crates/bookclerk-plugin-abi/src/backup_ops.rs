//! Host-facing backup/restore primitives (Send + Sync).
//!
//! [`crate::AdapterDatabaseSession`] methods are `?Send` Cap'n stubs. Library
//! backup orchestration stores a [`AdapterBackupOps`] handle so capture/restore
//! can call identity and catalog primitives without emitting dialect SQL.

use std::sync::Arc;

use crate::{DbIdentityHighWater, Result};

/// Snapshot / identity / restore hooks implemented by first-party adapters.
#[async_trait::async_trait]
pub trait AdapterBackupOps: Send + Sync {
    /// Identity high-water from adapter catalogs (`sqlite_sequence`, `bookclerk_identity`).
    async fn export_identity(&self) -> Result<Vec<DbIdentityHighWater>>;

    /// Restore identity high-water into adapter catalogs.
    async fn import_identity(&self, rows: &[DbIdentityHighWater]) -> Result<()>;

    /// User-visible relation names (excludes engine catalogs such as `sqlite_*`).
    async fn list_user_relations(&self) -> Result<Vec<String>>;

    /// Prepare an open restore transaction (SQLite deferred FK checks).
    async fn prepare_unit_restore(&self) -> Result<()>;

    /// Drop named user relations (Postgres `CASCADE` + identity companions).
    async fn drop_user_relations(&self, names: &[String]) -> Result<()>;

    /// Fail closed when the restore transaction still has FK violations.
    async fn assert_restore_constraints(&self) -> Result<()>;
}

/// Shared handle stored on backup/restore options.
pub type SharedAdapterBackupOps = Arc<dyn AdapterBackupOps>;
