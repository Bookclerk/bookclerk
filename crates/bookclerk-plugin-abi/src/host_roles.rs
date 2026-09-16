//! Host-private ABI roles (not exposed to third-party plugin authors).

use crate::Result;

/// Host-only interactive transaction on an adapter connection (SeaORM proxy).
#[async_trait::async_trait(?Send)]
pub trait AdapterTransaction {
    /// Typed statements on this open transaction (no second `BEGIN`).
    ///
    /// Proofs are required ([`crate::AdapterExecuteRequest::require_proofs`]).
    async fn execute(
        &self,
        request: crate::host_envelope::AdapterExecuteRequest,
    ) -> Result<crate::ExecuteReply>;

    /// Commit.
    async fn commit(&self) -> Result<()>;

    /// Rollback.
    async fn rollback(&self) -> Result<()>;

    /// Identity high-water from adapter catalogs on this transaction.
    async fn export_identity(&self) -> Result<Vec<crate::DbIdentityHighWater>> {
        Err(crate::PluginError::unsupported(
            "AdapterTransaction.exportIdentity",
        ))
    }

    /// Restore identity high-water into adapter catalogs on this transaction.
    async fn import_identity(&self, _rows: &[crate::DbIdentityHighWater]) -> Result<()> {
        Err(crate::PluginError::unsupported(
            "AdapterTransaction.importIdentity",
        ))
    }

    /// User-visible relation names visible to this transaction.
    async fn list_user_relations(&self) -> Result<Vec<String>> {
        Err(crate::PluginError::unsupported(
            "AdapterTransaction.listUserRelations",
        ))
    }

    /// Prepare this restore transaction (deferred FK checks).
    async fn prepare_unit_restore(&self) -> Result<()> {
        Err(crate::PluginError::unsupported(
            "AdapterTransaction.prepareUnitRestore",
        ))
    }

    /// Drop named user relations on this transaction.
    async fn drop_user_relations(&self, _names: &[String]) -> Result<()> {
        Err(crate::PluginError::unsupported(
            "AdapterTransaction.dropUserRelations",
        ))
    }

    /// Fail closed when this restore transaction still has FK violations.
    async fn assert_restore_constraints(&self) -> Result<()> {
        Err(crate::PluginError::unsupported(
            "AdapterTransaction.assertRestoreConstraints",
        ))
    }
}

/// Host-only view of an open adapter session (`begin` for interactive txn).
#[async_trait::async_trait(?Send)]
pub trait HostAdapterDatabaseSession {
    /// Opens a host-internal interactive transaction (SeaORM proxy).
    async fn begin(&self, isolation: crate::IsolationReq) -> Result<Box<dyn AdapterTransaction>>;

    /// Typed atomic execute on this host-private session.
    async fn execute(
        &self,
        request: crate::host_envelope::AdapterExecuteRequest,
    ) -> Result<crate::ExecuteReply> {
        let _ = request;
        Err(crate::PluginError::unsupported(
            "host execute not implemented",
        ))
    }
}
