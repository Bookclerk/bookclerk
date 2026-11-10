//! The `AdapterDatabaseSession` the D1 guest serves.
//!
//! It lives in the library, not the guest binary, so tests can run the
//! shared adapter conformance suite (`bookclerk-db-testkit`) against the
//! session the guest ships, on a mocked D1 HTTP API.

use async_trait::async_trait;
use bookclerk_plugin_abi::{AdapterExecuteRequest, IsolationReq};
use bookclerk_plugin_sdk::{
    AdapterDatabaseSession, DbBootstrap, DbCapabilities, ExecuteReply, ExecuteStream, PluginError,
};

/// D1 adapter session: typed atomic batches over the D1 HTTP API. D1 has no
/// interactive transactions, consistent backup reads or unit restore, so
/// those calls answer `unsupported`.
pub struct D1Session {
    /// Dedicated proxy of a binding open; `None` serves the shared library
    /// proxy installed at connect.
    dedicated: Option<crate::D1Proxy>,
}

impl D1Session {
    /// A session on `dedicated`, or on the shared library proxy when `None`.
    #[must_use]
    pub fn new(dedicated: Option<crate::D1Proxy>) -> Self {
        Self { dedicated }
    }

    /// The proxy this session runs on.
    fn proxy(&self) -> Result<crate::D1Proxy, PluginError> {
        if let Some(proxy) = &self.dedicated {
            return Ok(proxy.clone());
        }
        crate::shared_proxy().ok_or_else(|| PluginError::internal("d1 guest is not connected"))
    }
}

#[async_trait(?Send)]
impl AdapterDatabaseSession for D1Session {
    async fn capabilities(&self) -> Result<DbCapabilities, PluginError> {
        Ok(DbCapabilities::advertised_d1())
    }

    async fn bootstrap(&self) -> Result<DbBootstrap, PluginError> {
        Ok(DbBootstrap::sqlite())
    }

    async fn execute(&self, request: AdapterExecuteRequest) -> Result<ExecuteReply, PluginError> {
        request.require_proofs()?;
        if !matches!(request.isolation, IsolationReq::AtomicBatch) {
            return Err(PluginError::unsupported(
                "D1 supports AtomicBatch isolation only",
            ));
        }
        self.proxy()?
            .run_typed_atomic(&request.request, request.guest_receipt, &request.proofs)
            .await
            .map_err(crate::atomic::plugin_error_from_d1)
    }

    async fn execute_streamed(
        &self,
        request: AdapterExecuteRequest,
    ) -> Result<ExecuteStream, PluginError> {
        request.require_proofs()?;
        if !matches!(request.isolation, IsolationReq::AtomicBatch) {
            return Err(PluginError::unsupported(
                "D1 supports AtomicBatch isolation only",
            ));
        }
        let caps = DbCapabilities::advertised_d1().streamed().ok_or_else(|| {
            PluginError::unsupported("this adapter advertises no streamed result budget")
        })?;
        let reply = self
            .proxy()?
            .run_typed_atomic_with(
                &request.request,
                request.guest_receipt,
                &request.proofs,
                &caps,
            )
            .await
            .map_err(crate::atomic::plugin_error_from_d1)?;
        ExecuteStream::from_reply(&reply)
    }

    async fn export_identity(
        &self,
    ) -> Result<Vec<bookclerk_plugin_sdk::DbIdentityHighWater>, PluginError> {
        Err(PluginError::unsupported(
            "D1 does not advertise consistentBackupRead",
        ))
    }

    async fn import_identity(
        &self,
        _rows: &[bookclerk_plugin_sdk::DbIdentityHighWater],
    ) -> Result<(), PluginError> {
        Err(PluginError::unsupported(
            "D1 does not advertise atomicUnitRestore",
        ))
    }

    async fn list_user_relations(&self) -> Result<Vec<String>, PluginError> {
        Err(PluginError::unsupported(
            "D1 does not advertise consistentBackupRead",
        ))
    }

    async fn prepare_unit_restore(&self) -> Result<(), PluginError> {
        Err(PluginError::unsupported(
            "D1 does not advertise atomicUnitRestore",
        ))
    }

    async fn drop_user_relations(&self, _names: &[String]) -> Result<(), PluginError> {
        Err(PluginError::unsupported(
            "D1 does not advertise atomicUnitRestore",
        ))
    }

    async fn assert_restore_constraints(&self) -> Result<(), PluginError> {
        Err(PluginError::unsupported(
            "D1 does not advertise atomicUnitRestore",
        ))
    }
}
