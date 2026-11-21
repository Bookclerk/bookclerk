//! Cap'n Proto glue for public ingress: the `Origin` binding the host serves
//! and a relay plugin calls, and the `Ingress` entrypoint the plugin serves
//! and the host calls.
//!
//! The host side checks what crosses from the guest before it reaches the
//! library ([`crate::origin`]): the request head on `Origin.fetch` and the
//! origin `Ingress.start` reports. It checks the response head it sends back
//! the same way.

use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use tokio::io::AsyncRead;

use super::{
    async_read_from_byte_source, byte_source_from_async_read, from_capnp, read_error, write_error,
};
use crate::generated::{
    read_ingress_health_reply, read_ingress_start_reply, read_origin_header, read_origin_request,
    write_ingress_health_reply, write_ingress_start_reply, write_origin_header,
    write_origin_request, IngressHealth, IngressStarted, OriginHeader, OriginRequest,
};
use crate::origin::{
    check_origin_request, check_origin_response, parse_ingress_origin, CheckedOriginRequest,
    IngressOrigin,
};
use crate::plugin_capnp::{
    empty_reply, ingress as ingress_capnp, origin as origin_capnp, origin_fetch_reply,
};
use crate::roles::{Ingress, Origin, OriginReply};
use crate::{PluginError, Result};

/// Writes `headers` into a header list builder.
///
/// # Errors
///
/// A Cap'n Proto error when the list is too long or a field cannot be set.
fn write_headers(
    mut list: capnp::struct_list::Builder<'_, crate::plugin_capnp::origin_header::Owned>,
    headers: &[OriginHeader],
) -> capnp::Result<()> {
    for (i, header) in headers.iter().enumerate() {
        let index =
            u32::try_from(i).map_err(|_| capnp::Error::failed("too many headers".into()))?;
        write_origin_header(list.reborrow().get(index), header)?;
    }
    Ok(())
}

/// Builds the wire request head from a checked one.
fn wire_request(request: &CheckedOriginRequest) -> OriginRequest {
    OriginRequest {
        method: request.method.clone(),
        target: request.target.clone(),
        headers: request.headers.clone(),
        client_address: request.client.map(|ip| ip.to_string()),
    }
}

/// Host-side server for the [`Origin`] binding handed to a relay plugin on
/// `Bindings.origin`. Every request head is checked here before it reaches
/// the library, and every response head before it leaves.
pub(super) struct OriginServer {
    /// Host implementation (the library's router).
    pub(super) inner: Arc<dyn Origin>,
    /// Stream window for the request and response bodies.
    pub(super) window: u32,
}

impl origin_capnp::Server for OriginServer {
    async fn fetch(
        self: Rc<Self>,
        params: origin_capnp::FetchParams,
        mut results: origin_capnp::FetchResults,
    ) -> capnp::Result<()> {
        let p = params.get()?;
        let request = read_origin_request(p.get_request()?)?;
        let body = if p.has_body() {
            Some(async_read_from_byte_source(p.get_body()?, self.window))
        } else {
            None
        };
        let outcome = match check_origin_request(&request) {
            Ok(checked) => self.inner.fetch(checked, body).await.and_then(|reply| {
                check_origin_response(reply.status, &reply.headers).map(|()| reply)
            }),
            Err(err) => Err(err),
        };
        let result = results.get().init_result();
        match outcome {
            Ok(reply) => {
                let count = u32::try_from(reply.headers.len())
                    .map_err(|_| capnp::Error::failed("too many headers".into()))?;
                let mut ok = result.init_ok();
                ok.set_status(reply.status);
                write_headers(ok.reborrow().init_headers(count), &reply.headers)?;
                if let Some(body) = reply.body {
                    ok.set_body(byte_source_from_async_read(body, self.window));
                }
            }
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }
}

/// Guest-side client for the [`Origin`] binding (what a native relay plugin
/// calls).
pub struct OriginClient {
    /// The host's capability.
    client: origin_capnp::Client,
    /// Stream window for the request and response bodies.
    window: u32,
}

impl OriginClient {
    /// Wraps the `Bindings.origin` capability.
    #[must_use]
    pub fn new(client: origin_capnp::Client, window: u32) -> Self {
        Self { client, window }
    }
}

#[async_trait::async_trait(?Send)]
impl Origin for OriginClient {
    async fn fetch(
        &self,
        request: CheckedOriginRequest,
        body: Option<Pin<Box<dyn AsyncRead + Send>>>,
    ) -> Result<OriginReply> {
        let mut req = self.client.fetch_request();
        {
            let mut p = req.get();
            write_origin_request(p.reborrow().init_request(), &wire_request(&request))
                .map_err(from_capnp)?;
            if let Some(body) = body {
                p.set_body(byte_source_from_async_read(body, self.window));
            }
        }
        let reply = req.send().promise.await.map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            origin_fetch_reply::Ok(ok) => {
                let ok = ok.map_err(from_capnp)?;
                let mut headers = Vec::new();
                for header in ok.get_headers().map_err(from_capnp)?.iter() {
                    headers.push(read_origin_header(header).map_err(from_capnp)?);
                }
                let body = if ok.has_body() {
                    Some(async_read_from_byte_source(
                        ok.get_body().map_err(from_capnp)?,
                        self.window,
                    ))
                } else {
                    None
                };
                Ok(OriginReply {
                    status: ok.get_status(),
                    headers,
                    body,
                })
            }
            origin_fetch_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
        }
    }
}

/// Guest-side server for the `ingress` entrypoint.
pub(super) struct IngressServer {
    /// Guest implementation.
    pub(super) inner: Arc<dyn Ingress>,
}

impl ingress_capnp::Server for IngressServer {
    async fn start(
        self: Rc<Self>,
        _params: ingress_capnp::StartParams,
        mut results: ingress_capnp::StartResults,
    ) -> capnp::Result<()> {
        let outcome = self.inner.start().await;
        write_ingress_start_reply(results.get().init_result(), &outcome)
    }

    async fn health(
        self: Rc<Self>,
        _params: ingress_capnp::HealthParams,
        mut results: ingress_capnp::HealthResults,
    ) -> capnp::Result<()> {
        let outcome = self.inner.health().await;
        write_ingress_health_reply(results.get().init_result(), &outcome)
    }

    async fn stop(
        self: Rc<Self>,
        _params: ingress_capnp::StopParams,
        mut results: ingress_capnp::StopResults,
    ) -> capnp::Result<()> {
        let mut result = results.get().init_result();
        match self.inner.stop().await {
            Ok(()) => result.set_ok(()),
            Err(err) => write_error(result.init_err(), &err),
        }
        Ok(())
    }
}

/// Host-side client for a plugin's `ingress` entrypoint.
#[derive(Clone)]
pub struct IngressClient {
    /// The guest's capability.
    client: ingress_capnp::Client,
}

impl IngressClient {
    /// Wraps the `Entrypoints.ingress` capability.
    #[must_use]
    pub fn new(client: ingress_capnp::Client) -> Self {
        Self { client }
    }

    /// Asks the plugin to bring its public address up and checks the origin
    /// it reports.
    ///
    /// # Returns
    ///
    /// The origin it serves ([`parse_ingress_origin`]).
    ///
    /// # Errors
    ///
    /// The plugin's error, the RPC error, or `invalid_params` when the
    /// reported origin is not an `https://` host name.
    pub async fn start_checked(&self) -> Result<IngressOrigin> {
        parse_ingress_origin(&Ingress::start(self).await?)
    }
}

#[async_trait::async_trait(?Send)]
impl Ingress for IngressClient {
    async fn start(&self) -> Result<IngressStarted> {
        let reply = self
            .client
            .start_request()
            .send()
            .promise
            .await
            .map_err(from_capnp)?;
        read_ingress_start_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
        .map_err(from_capnp)?
    }

    /// Asks the plugin whether its public address is serving.
    ///
    /// # Errors
    ///
    /// The plugin's error or the RPC error.
    async fn health(&self) -> Result<IngressHealth> {
        let reply = self
            .client
            .health_request()
            .send()
            .promise
            .await
            .map_err(from_capnp)?;
        read_ingress_health_reply(
            reply
                .get()
                .map_err(from_capnp)?
                .get_result()
                .map_err(from_capnp)?,
        )
        .map_err(from_capnp)?
    }

    /// Asks the plugin to take its public address down.
    ///
    /// # Errors
    ///
    /// The plugin's error or the RPC error.
    async fn stop(&self) -> Result<()> {
        let reply = self
            .client
            .stop_request()
            .send()
            .promise
            .await
            .map_err(from_capnp)?;
        let result = reply
            .get()
            .map_err(from_capnp)?
            .get_result()
            .map_err(from_capnp)?;
        match result.which().map_err(from_capnp)? {
            empty_reply::Ok(()) => Ok(()),
            empty_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
        }
    }
}

/// Fails closed: an `Origin` that refuses everything, for hosts that grant
/// no relay ingress. Kept for tests and in-process hosts.
pub struct NoOrigin;

#[async_trait::async_trait(?Send)]
impl Origin for NoOrigin {
    async fn fetch(
        &self,
        _request: CheckedOriginRequest,
        _body: Option<Pin<Box<dyn AsyncRead + Send>>>,
    ) -> Result<OriginReply> {
        Err(PluginError::unavailable("no relay ingress is active"))
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
#[path = "rpc_origin_tests.rs"]
mod tests;
