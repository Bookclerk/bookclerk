//! Host-private ABI roles (not exposed to third-party plugin authors).

use crate::Result;

/// Host-only interactive transaction on an adapter connection (SeaORM proxy).
#[async_trait::async_trait(?Send)]
pub trait AdapterTransaction {
    /// Typed statements on this open transaction (no second `BEGIN`).
    async fn execute(&self, request: crate::ExecuteRequest) -> Result<crate::ExecuteReply>;

    /// Typed execute with a host-only guest receipt finalize hint.
    ///
    /// Default forwards [`Self::execute`] when the envelope has no persist
    /// hint and no proofs. Nested guest-receipt batches must override this.
    async fn execute_envelope(
        &self,
        envelope: crate::host_envelope::HostExecuteEnvelope,
    ) -> Result<crate::ExecuteReply> {
        if envelope.guest_receipt.is_absent() && envelope.proofs.is_empty() {
            return self.execute(envelope.request).await;
        }
        Err(crate::PluginError::unsupported(
            "nested host executeEnvelope not implemented",
        ))
    }

    /// Commit.
    async fn commit(&self) -> Result<()>;

    /// Rollback.
    async fn rollback(&self) -> Result<()>;
}

/// Host-only view of an open adapter session (`begin` for interactive txn).
#[async_trait::async_trait(?Send)]
pub trait HostAdapterDatabaseSession {
    /// Opens a host-internal interactive transaction (SeaORM proxy).
    async fn begin(&self) -> Result<Box<dyn AdapterTransaction>>;

    /// Typed execute with a host-only guest receipt finalize hint.
    async fn execute_envelope(
        &self,
        envelope: crate::host_envelope::HostExecuteEnvelope,
    ) -> Result<crate::ExecuteReply> {
        let _ = envelope;
        Err(crate::PluginError::unsupported(
            "host executeEnvelope not implemented",
        ))
    }
}
