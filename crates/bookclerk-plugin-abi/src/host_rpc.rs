//! Host-private Cap'n Proto RPC adapters (interactive adapter transactions).

#![allow(clippy::missing_docs_in_private_items)]
#![allow(clippy::arc_with_non_send_sync)]

use std::rc::Rc;
use std::sync::Arc;

use capnp::capability::FromClientHook;

use crate::host_roles::{AdapterTransaction, HostAdapterDatabaseSession};
use crate::plugin_capnp::adapter_database_session as adapter_database_session_capnp;
use crate::plugin_host_capnp::{
    adapter_transaction as adapter_transaction_capnp, adapter_transaction_reply,
    host_adapter_database_session as host_adapter_database_session_capnp,
};
use crate::rpc::{from_capnp, read_error};
use crate::{PluginError, Result};

/// Decodes an empty success reply or guest error union.
///
/// # Errors
///
/// Returns a decode failure or the guest [`PluginError`] on `err`.
fn read_empty(reader: crate::plugin_capnp::empty_reply::Reader<'_>) -> Result<()> {
    match reader.which().map_err(from_capnp)? {
        crate::plugin_capnp::empty_reply::Ok(()) => Ok(()),
        crate::plugin_capnp::empty_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
    }
}

fn write_error(mut out: crate::plugin_capnp::plugin_error::Builder<'_>, err: &PluginError) {
    out.set_code(err.wire_str());
    out.set_message(&err.message);
}

struct AdapterTransactionServer {
    inner: Arc<dyn AdapterTransaction>,
}

/// Exports an open guest transaction as a host-private Cap'n Proto capability.
pub fn new_adapter_transaction_client(
    inner: Arc<dyn AdapterTransaction>,
) -> adapter_transaction_capnp::Client {
    capnp_rpc::new_client(AdapterTransactionServer { inner })
}

impl adapter_transaction_capnp::Server for AdapterTransactionServer {
    async fn execute(
        self: Rc<Self>,
        params: adapter_transaction_capnp::ExecuteParams,
        mut results: adapter_transaction_capnp::ExecuteResults,
    ) -> capnp::Result<()> {
        let request = params
            .get()?
            .get_request()
            .map_err(|err| capnp::Error::failed(err.to_string()))
            .and_then(|r| {
                crate::db_rpc::read_execute_request(r)
                    .map_err(|err| capnp::Error::failed(err.to_string()))
            })?;
        crate::db_rpc::write_execute_result_reply(
            results.get().init_result(),
            self.inner.execute(request).await,
        );
        Ok(())
    }

    async fn execute_envelope(
        self: Rc<Self>,
        params: adapter_transaction_capnp::ExecuteEnvelopeParams,
        mut results: adapter_transaction_capnp::ExecuteEnvelopeResults,
    ) -> capnp::Result<()> {
        let envelope = params
            .get()?
            .get_envelope()
            .map_err(|err| capnp::Error::failed(err.to_string()))
            .and_then(|r| {
                crate::db_rpc::read_host_execute_envelope(r)
                    .map_err(|err| capnp::Error::failed(err.to_string()))
            })?;
        crate::db_rpc::write_execute_result_reply(
            results.get().init_result(),
            self.inner.execute_envelope(envelope).await,
        );
        Ok(())
    }

    async fn commit(
        self: Rc<Self>,
        _params: adapter_transaction_capnp::CommitParams,
        mut results: adapter_transaction_capnp::CommitResults,
    ) -> capnp::Result<()> {
        let mut result = results.get().init_result();
        match self.inner.commit().await {
            Ok(()) => result.set_ok(()),
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }

    async fn rollback(
        self: Rc<Self>,
        _params: adapter_transaction_capnp::RollbackParams,
        mut results: adapter_transaction_capnp::RollbackResults,
    ) -> capnp::Result<()> {
        let mut result = results.get().init_result();
        match self.inner.rollback().await {
            Ok(()) => result.set_ok(()),
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }
}

/// Host-side client for interactive transactions on an adapter session.
pub struct HostAdapterTransactionClient {
    client: adapter_transaction_capnp::Client,
}

#[async_trait::async_trait(?Send)]
impl AdapterTransaction for HostAdapterTransactionClient {
    async fn execute(&self, request: crate::ExecuteRequest) -> Result<crate::ExecuteReply> {
        let mut req = self.client.execute_request();
        crate::db_rpc::write_execute_request(req.get().init_request(), &request)?;
        let reply = req.send().promise.await.map_err(from_capnp)?;
        crate::db_rpc::read_execute_result_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }

    async fn execute_envelope(
        &self,
        envelope: crate::host_envelope::HostExecuteEnvelope,
    ) -> Result<crate::ExecuteReply> {
        let mut req = self.client.execute_envelope_request();
        crate::db_rpc::write_host_execute_envelope(req.get().init_envelope(), &envelope)?;
        let reply = req.send().promise.await.map_err(from_capnp)?;
        crate::db_rpc::read_execute_result_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }

    async fn commit(&self) -> Result<()> {
        let req = self.client.commit_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_empty(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }

    async fn rollback(&self) -> Result<()> {
        let req = self.client.rollback_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        read_empty(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
}

/// Host-side client for `begin` on an open adapter session capability.
pub struct HostAdapterDatabaseSessionClient {
    client: host_adapter_database_session_capnp::Client,
}

impl HostAdapterDatabaseSessionClient {
    /// Casts a public adapter session capability to the host-private interface.
    pub fn from_session_client(client: adapter_database_session_capnp::Client) -> Self {
        Self {
            client: client
                .client
                .cast_to::<host_adapter_database_session_capnp::Client>(),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl HostAdapterDatabaseSession for HostAdapterDatabaseSessionClient {
    async fn begin(&self) -> Result<Box<dyn AdapterTransaction>> {
        let req = self.client.begin_request();
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            adapter_transaction_reply::Ok(txn) => Ok(Box::new(HostAdapterTransactionClient {
                client: txn.map_err(from_capnp)?,
            })),
            adapter_transaction_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
        }
    }

    async fn execute_envelope(
        &self,
        envelope: crate::host_envelope::HostExecuteEnvelope,
    ) -> Result<crate::ExecuteReply> {
        let mut req = self.client.execute_envelope_request();
        crate::db_rpc::write_host_execute_envelope(req.get().init_envelope(), &envelope)?;
        let reply = req.send().promise.await.map_err(from_capnp)?;
        crate::db_rpc::read_execute_result_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
}
