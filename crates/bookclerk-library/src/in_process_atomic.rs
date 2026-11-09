//! Named atomic operations on the local SQLite connection (no plugin RPC).
//!
//! A store opened by a local tool, or by a test that does not go through
//! the test kit, has no database plugin to send named operations to. This
//! runner applies the same compiled plans on its own connection, as one
//! native transaction each, and holds them to the shipped SQLite adapter's
//! advertisement so a plan the adapter would refuse fails here too.

use async_trait::async_trait;
use bookclerk_plugin_abi::DbCapabilities;
use sea_orm::DatabaseConnection;

use crate::atomic_ops::DbAtomicResult;
use crate::error::Result;
use crate::named_atomic::{NamedAtomicBackend, NamedAtomicRunner};
use crate::sql_plan::{validate_plan, CompiledAtomic};

/// Runs compiled named plans on a local SeaORM SQLite connection.
#[derive(Debug, Clone)]
pub struct InProcessSqlite {
    /// Open library connection (already migrated).
    db: DatabaseConnection,
}

impl InProcessSqlite {
    /// The connection the plans run on.
    #[must_use]
    pub fn connection(&self) -> &DatabaseConnection {
        &self.db
    }
}

#[async_trait]
impl NamedAtomicRunner for InProcessSqlite {
    async fn run_compiled(
        &self,
        compiled: CompiledAtomic,
        _operation_id: String,
    ) -> Result<DbAtomicResult> {
        validate_plan(&compiled, &DbCapabilities::advertised_sqlite())?;
        crate::db_atomic::execute_db_atomic(&self.db, compiled).await
    }
}

/// The [`crate::AtomicTxnBackend`] over a local SQLite connection.
pub type InProcessSqliteAtomic = NamedAtomicBackend<InProcessSqlite>;

impl InProcessSqliteAtomic {
    /// The backend over `db`, an open and migrated library connection.
    #[must_use]
    pub fn on(db: DatabaseConnection) -> Self {
        Self::new(InProcessSqlite { db })
    }
}
