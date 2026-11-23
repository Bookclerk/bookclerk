//! Cap'n Proto glue for guided setup: the `Provisioning` entrypoint a
//! plugin serves and the host calls.
//!
//! The host checks what the plugin returns ([`crate::provision`]) after the
//! call, against the manifest's `[provisioning] outputs`, so this glue only
//! moves the request and the reply.

use std::rc::Rc;
use std::sync::Arc;

use super::from_capnp;
use crate::generated::{
    read_deprovision_request, read_provision_reply, read_provision_request,
    write_deprovision_request, write_provision_reply, write_provision_request, DeprovisionRequest,
    ProvisionRequest, Provisioned,
};
use crate::plugin_capnp::provisioning as provisioning_capnp;
use crate::roles::Provisioning;
use crate::Result;

/// Guest-side server for the `provisioning` entrypoint.
pub(super) struct ProvisioningServer {
    /// Guest implementation.
    pub(super) inner: Arc<dyn Provisioning>,
}

impl provisioning_capnp::Server for ProvisioningServer {
    async fn provision(
        self: Rc<Self>,
        params: provisioning_capnp::ProvisionParams,
        mut results: provisioning_capnp::ProvisionResults,
    ) -> capnp::Result<()> {
        let request = read_provision_request(params.get()?.get_request()?)?;
        let outcome = self.inner.provision(request).await;
        write_provision_reply(results.get().init_result(), &outcome)
    }

    async fn deprovision(
        self: Rc<Self>,
        params: provisioning_capnp::DeprovisionParams,
        mut results: provisioning_capnp::DeprovisionResults,
    ) -> capnp::Result<()> {
        let request = read_deprovision_request(params.get()?.get_request()?)?;
        let mut result = results.get().init_result();
        match self.inner.deprovision(request).await {
            Ok(()) => result.set_ok(()),
            Err(err) => super::write_error(result.init_err(), &err),
        }
        Ok(())
    }
}

/// Host-side client for a plugin's `provisioning` entrypoint.
#[derive(Clone)]
pub struct ProvisioningClient {
    /// The guest's capability.
    client: provisioning_capnp::Client,
}

impl ProvisioningClient {
    /// Wraps the `Entrypoints.provisioning` capability.
    #[must_use]
    pub fn new(client: provisioning_capnp::Client) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait(?Send)]
impl Provisioning for ProvisioningClient {
    /// Runs the plugin's guided setup with the operator's grant.
    ///
    /// # Errors
    ///
    /// The plugin's error or the RPC error.
    async fn provision(&self, request: ProvisionRequest) -> Result<Provisioned> {
        let mut call = self.client.provision_request();
        write_provision_request(call.get().init_request(), &request).map_err(from_capnp)?;
        let reply = call.send().promise.await.map_err(from_capnp)?;
        read_provision_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
        .map_err(from_capnp)?
    }

    /// Asks the plugin to remove what its guided setup created.
    ///
    /// # Errors
    ///
    /// The plugin's error or the RPC error.
    async fn deprovision(&self, request: DeprovisionRequest) -> Result<()> {
        let mut call = self.client.deprovision_request();
        write_deprovision_request(call.get().init_request(), &request).map_err(from_capnp)?;
        let reply = call.send().promise.await.map_err(from_capnp)?;
        super::read_empty(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc, clippy::missing_errors_doc)]
mod tests {
    use super::*;
    use crate::{PluginError, PluginErrorCode, ProvisionGrant, ProvisionGrantKind};
    use std::cell::RefCell;

    /// Records what it was asked and answers fixed replies.
    struct Recorder {
        seen: Rc<RefCell<Vec<String>>>,
    }

    #[async_trait::async_trait(?Send)]
    impl Provisioning for Recorder {
        async fn provision(&self, request: ProvisionRequest) -> Result<Provisioned> {
            self.seen.borrow_mut().push(format!(
                "provision {:?} {}",
                request.grant.kind, request.inputs_json
            ));
            if request.inputs_json == "refuse" {
                return Err(PluginError::forbidden("the token cannot create buckets"));
            }
            Ok(Provisioned {
                settings_json: r#"{"bucket":"books"}"#.into(),
                secrets: Vec::new(),
                summary: vec!["Created bucket books".into()],
            })
        }

        async fn deprovision(&self, request: DeprovisionRequest) -> Result<()> {
            self.seen
                .borrow_mut()
                .push(format!("deprovision {:?}", request.grant.kind));
            Ok(())
        }
    }

    /// A guest that keeps the default `deprovision`.
    struct NoDeprovision;

    #[async_trait::async_trait(?Send)]
    impl Provisioning for NoDeprovision {
        async fn provision(&self, _request: ProvisionRequest) -> Result<Provisioned> {
            Ok(Provisioned::default())
        }
    }

    fn client_for(inner: Arc<dyn Provisioning>) -> ProvisioningClient {
        ProvisioningClient::new(capnp_rpc::new_client(ProvisioningServer { inner }))
    }

    fn grant(kind: ProvisionGrantKind) -> ProvisionGrant {
        ProvisionGrant {
            kind,
            token: Some("token".into()),
            expires_at_unix_ms: None,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn provision_and_deprovision_cross_the_connection() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let seen = Rc::new(RefCell::new(Vec::new()));
                let client = client_for(Arc::new(Recorder { seen: seen.clone() }));
                let done = client
                    .provision(ProvisionRequest {
                        grant: grant(ProvisionGrantKind::ApiToken),
                        inputs_json: "{}".into(),
                    })
                    .await
                    .expect("provisioned");
                assert_eq!(done.summary, ["Created bucket books"]);
                let refused = client
                    .provision(ProvisionRequest {
                        grant: grant(ProvisionGrantKind::OauthAccessToken),
                        inputs_json: "refuse".into(),
                    })
                    .await
                    .expect_err("refused");
                assert_eq!(refused.code, PluginErrorCode::Forbidden);
                client
                    .deprovision(DeprovisionRequest {
                        grant: grant(ProvisionGrantKind::None),
                    })
                    .await
                    .expect("deprovisioned");
                assert_eq!(
                    *seen.borrow(),
                    [
                        "provision ApiToken {}",
                        "provision OauthAccessToken refuse",
                        "deprovision None",
                    ]
                );
                let unsupported = client_for(Arc::new(NoDeprovision))
                    .deprovision(DeprovisionRequest::default())
                    .await
                    .expect_err("unsupported");
                assert_eq!(unsupported.code, PluginErrorCode::Unsupported);
            })
            .await;
    }
}
