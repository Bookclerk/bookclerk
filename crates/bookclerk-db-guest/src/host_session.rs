//! Host-private adapter session helpers (first-party database guests only).

#![allow(clippy::missing_docs_in_private_items)]

use bookclerk_plugin_abi::{AdapterExecuteRequest, AdapterTransaction, HostAdapterDatabaseSession};
use bookclerk_plugin_abi::{ExecuteReply, IsolationReq, Result};
use sea_orm::DatabaseConnection;

use crate::session::{
    guest_assert_restore_constraints_on_txn, guest_begin_isolated, guest_begin_on_isolated,
    guest_commit, guest_drop_user_relations_on_txn, guest_execute_atomic, guest_execute_atomic_on,
    guest_export_identity_on_txn, guest_import_identity_on_txn, guest_list_user_relations_on_txn,
    guest_prepare_unit_restore_on_txn, guest_rollback,
};
use bookclerk_plugin_sdk::database_adapter::plugin_error_from_engine;

/// In-process SeaORM transaction bridge for host `begin` RPC (shared library connection).
pub struct GuestHostAdapterSession;

#[async_trait::async_trait(?Send)]
impl HostAdapterDatabaseSession for GuestHostAdapterSession {
    async fn begin(&self, isolation: IsolationReq) -> Result<Box<dyn AdapterTransaction>> {
        let txn_id = guest_begin_isolated(None, isolation)
            .await
            .map_err(plugin_error_from_engine)?;
        Ok(Box::new(GuestHostAdapterTransaction { txn_id }))
    }

    async fn execute(&self, request: AdapterExecuteRequest) -> Result<ExecuteReply> {
        guest_execute_atomic(request)
            .await
            .map_err(plugin_error_from_engine)
    }
}

struct GuestHostAdapterTransaction {
    txn_id: String,
}

#[async_trait::async_trait(?Send)]
impl AdapterTransaction for GuestHostAdapterTransaction {
    async fn execute(&self, request: AdapterExecuteRequest) -> Result<ExecuteReply> {
        request.require_proofs()?;
        match request.isolation {
            IsolationReq::ConsistentSnapshot => {
                return Err(bookclerk_plugin_abi::PluginError::unsupported(
                    "ConsistentSnapshot requires a dedicated capture session",
                ));
            }
            IsolationReq::AtomicBatch | IsolationReq::NestedSavepoint => {}
        }
        crate::session::guest_execute_atomic_on_txn_envelope(self.txn_id.clone(), request).await
    }

    async fn commit(&self) -> Result<()> {
        guest_commit(self.txn_id.clone())
            .await
            .map_err(plugin_error_from_engine)
    }

    async fn rollback(&self) -> Result<()> {
        guest_rollback(self.txn_id.clone())
            .await
            .map_err(plugin_error_from_engine)
    }

    async fn export_identity(&self) -> Result<Vec<bookclerk_plugin_abi::DbIdentityHighWater>> {
        guest_export_identity_on_txn(self.txn_id.clone())
            .await
            .map_err(plugin_error_from_engine)
    }

    async fn import_identity(
        &self,
        rows: &[bookclerk_plugin_abi::DbIdentityHighWater],
    ) -> Result<()> {
        guest_import_identity_on_txn(self.txn_id.clone(), rows.to_vec())
            .await
            .map_err(plugin_error_from_engine)
    }

    async fn list_user_relations(&self) -> Result<Vec<String>> {
        guest_list_user_relations_on_txn(self.txn_id.clone())
            .await
            .map_err(plugin_error_from_engine)
    }

    async fn prepare_unit_restore(&self) -> Result<()> {
        guest_prepare_unit_restore_on_txn(self.txn_id.clone())
            .await
            .map_err(plugin_error_from_engine)
    }

    async fn drop_user_relations(&self, names: &[String]) -> Result<()> {
        guest_drop_user_relations_on_txn(self.txn_id.clone(), names.to_vec())
            .await
            .map_err(plugin_error_from_engine)
    }

    async fn assert_restore_constraints(&self) -> Result<()> {
        guest_assert_restore_constraints_on_txn(self.txn_id.clone())
            .await
            .map_err(plugin_error_from_engine)
    }
}

/// Returns the default host adapter session on the shared guest connection.
pub fn host_session() -> GuestHostAdapterSession {
    GuestHostAdapterSession
}

/// Host-private envelope session bound to one dedicated connection (named bindings).
pub struct BoundGuestHostAdapterSession {
    conn: DatabaseConnection,
}

#[async_trait::async_trait(?Send)]
impl HostAdapterDatabaseSession for BoundGuestHostAdapterSession {
    async fn begin(&self, isolation: IsolationReq) -> Result<Box<dyn AdapterTransaction>> {
        let txn_id = guest_begin_on_isolated(self.conn.clone(), isolation)
            .await
            .map_err(plugin_error_from_engine)?;
        Ok(Box::new(GuestHostAdapterTransaction { txn_id }))
    }

    async fn execute(&self, request: AdapterExecuteRequest) -> Result<ExecuteReply> {
        guest_execute_atomic_on(&self.conn, request).await
    }
}

/// Host envelope session that persists guest receipts on `conn`.
#[must_use]
pub fn host_session_on(conn: DatabaseConnection) -> BoundGuestHostAdapterSession {
    BoundGuestHostAdapterSession { conn }
}
