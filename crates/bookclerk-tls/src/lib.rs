//! The one TLS backend for the Bookclerk hosts.
//!
//! Every first-party TLS client and server in the hosts uses rustls with the
//! aws-lc-rs crypto provider, which also brings the hybrid post-quantum key
//! exchange (`X25519MLKEM768`) rustls prefers. This crate is the single place
//! that names the provider, so no crate falls back to another one (ring or
//! OpenSSL) and the build compiles one crypto library.
//!
//! - [`crypto_provider`] hands the provider to code that builds a rustls
//!   `ClientConfig` or `ServerConfig` itself (the node API listener and
//!   client).
//! - [`ureq`] is the `ureq` crate as host crates use it: `use bookclerk_tls::ureq;`
//!   and then `ureq::get(url)` as usual. Its request functions send through
//!   [`http_agent`] instead of `ureq`'s own default agent, which has no
//!   provider (the workspace builds `ureq` with `rustls-no-provider`) and would
//!   panic on its first HTTPS request. Host crates depend on this crate rather
//!   than on `ureq` directly, so that agent is out of reach.
//! - [`http_agent`] is the shared blocking HTTP client behind those functions.
//! - [`ureq_tls_config`] is the same TLS setting for crates that build a
//!   custom `ureq` agent.
//!
//! See `docs/internals/tls.md`.

use std::sync::{Arc, OnceLock};

use rustls::crypto::{aws_lc_rs, CryptoProvider};

/// The rustls crypto provider every host TLS connection uses: aws-lc-rs with
/// its default cipher suites and key exchange groups.
///
/// # Returns
///
/// A shared handle to the provider. Every call returns the same instance, so
/// rustls and `ureq` can pool configurations built from it.
#[must_use]
pub fn crypto_provider() -> Arc<CryptoProvider> {
    /// The provider, built on first use.
    static PROVIDER: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    Arc::clone(PROVIDER.get_or_init(|| Arc::new(aws_lc_rs::default_provider())))
}

/// `ureq` TLS settings that use [`crypto_provider`] and Mozilla's root
/// certificates (`webpki-roots`), as `ureq`'s default agent does.
///
/// # Returns
///
/// A TLS configuration to pass to `ureq::config::ConfigBuilder::tls_config`.
#[must_use]
pub fn ureq_tls_config() -> ureq::tls::TlsConfig {
    ureq::tls::TlsConfig::builder()
        .unversioned_rustls_crypto_provider(crypto_provider())
        .build()
}

/// The shared blocking HTTP client for the hosts.
///
/// It has `ureq`'s default settings apart from TLS, which uses
/// [`ureq_tls_config`]. Callers that need other settings (timeouts, status
/// handling) set them per request, or build their own agent with
/// [`ureq_tls_config`].
///
/// # Returns
///
/// A clone of one process-wide agent; clones share its connection pool.
#[must_use]
pub fn http_agent() -> ureq::Agent {
    /// The agent, built on first use.
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT
        .get_or_init(|| {
            ureq::Agent::config_builder()
                .tls_config(ureq_tls_config())
                .build()
                .new_agent()
        })
        .clone()
}

/// The `ureq` crate with the hosts' TLS: everything `ureq` exports, except
/// that the request functions (`get`, `post`, …), `run` and `agent` use
/// [`http_agent`].
///
/// Import it in place of the crate (`use bookclerk_tls::ureq;`); call sites
/// then read like plain `ureq`. Build any other agent from
/// `Agent::config_builder()` with [`ureq_tls_config`], never from
/// `Agent::new_with_defaults()`, which has no provider.
pub mod ureq {
    pub use ::ureq::*;

    use ::ureq::http::Uri;
    use ::ureq::typestate::{WithBody, WithoutBody};

    /// Defines one request function that sends through [`super::http_agent`].
    macro_rules! request_fn {
        ($(#[$doc:meta])* $name:ident -> $body:ty) => {
            $(#[$doc])*
            ///
            /// # Arguments
            ///
            /// * `uri` - The request target, anything that converts to a URI.
            ///
            /// # Returns
            ///
            /// A request builder on the shared agent.
            pub fn $name<T>(uri: T) -> RequestBuilder<$body>
            where
                Uri: TryFrom<T>,
                <Uri as TryFrom<T>>::Error: Into<::ureq::http::Error>,
            {
                super::http_agent().$name(uri)
            }
        };
    }

    /// The shared agent, in place of `ureq::agent()`, whose fresh default
    /// agent has no crypto provider.
    ///
    /// # Returns
    ///
    /// A clone of [`super::http_agent`].
    #[must_use]
    pub fn agent() -> Agent {
        super::http_agent()
    }

    /// Sends a prebuilt `http::Request` on the shared agent, in place of
    /// `ureq::run`, which uses a fresh default agent.
    ///
    /// # Arguments
    ///
    /// * `request` - The request to send.
    ///
    /// # Returns
    ///
    /// The response.
    ///
    /// # Errors
    ///
    /// Any [`Error`] the transfer raises.
    pub fn run(
        request: ::ureq::http::Request<impl AsSendBody>,
    ) -> Result<::ureq::http::Response<Body>, Error> {
        super::http_agent().run(request)
    }

    request_fn!(/// Starts a `GET` request.
        get -> WithoutBody);
    request_fn!(/// Starts a `POST` request.
        post -> WithBody);
    request_fn!(/// Starts a `PUT` request.
        put -> WithBody);
    request_fn!(/// Starts a `DELETE` request.
        delete -> WithoutBody);
    request_fn!(/// Starts a `HEAD` request.
        head -> WithoutBody);
    request_fn!(/// Starts an `OPTIONS` request.
        options -> WithoutBody);
    request_fn!(/// Starts a `CONNECT` request.
        connect -> WithoutBody);
    request_fn!(/// Starts a `PATCH` request.
        patch -> WithBody);
    request_fn!(/// Starts a `TRACE` request.
        trace -> WithoutBody);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_provider_is_shared_and_offers_post_quantum_key_exchange() {
        let provider = crypto_provider();
        assert!(Arc::ptr_eq(&provider, &crypto_provider()));
        let groups: Vec<String> = provider
            .kx_groups
            .iter()
            .map(|group| format!("{:?}", group.name()))
            .collect();
        assert!(
            groups.iter().any(|name| name == "X25519MLKEM768"),
            "aws-lc-rs offers the hybrid post-quantum group: {groups:?}"
        );
    }

    #[test]
    fn the_ureq_settings_carry_the_provider() {
        let config = ureq_tls_config();
        let configured = config
            .unversioned_rustls_crypto_provider()
            .as_ref()
            .map(Arc::clone);
        assert!(configured.is_some_and(|p| Arc::ptr_eq(&p, &crypto_provider())));
    }

    #[test]
    fn every_request_function_builds_its_method() {
        use ::ureq::http::Method;
        let url = "https://example.invalid/";
        let methods = [
            (super::ureq::get(url).method_ref().cloned(), Method::GET),
            (
                super::ureq::delete(url).method_ref().cloned(),
                Method::DELETE,
            ),
            (super::ureq::head(url).method_ref().cloned(), Method::HEAD),
            (
                super::ureq::options(url).method_ref().cloned(),
                Method::OPTIONS,
            ),
            (
                super::ureq::connect(url).method_ref().cloned(),
                Method::CONNECT,
            ),
            (super::ureq::trace(url).method_ref().cloned(), Method::TRACE),
            (super::ureq::post(url).method_ref().cloned(), Method::POST),
            (super::ureq::put(url).method_ref().cloned(), Method::PUT),
            (super::ureq::patch(url).method_ref().cloned(), Method::PATCH),
        ];
        for (got, want) in methods {
            assert_eq!(got, Some(want));
        }
    }

    #[test]
    fn the_facade_agent_and_run_use_the_shared_agent() {
        let agent = super::ureq::agent();
        let configured = agent
            .config()
            .tls_config()
            .unversioned_rustls_crypto_provider()
            .as_ref()
            .map(Arc::clone);
        assert!(configured.is_some_and(|p| Arc::ptr_eq(&p, &crypto_provider())));

        // An unroutable request fails in the transport, not on a missing
        // crypto provider (which would panic).
        let request = ::ureq::http::Request::get("http://127.0.0.1:9/")
            .body(())
            .map_err(|err| err.to_string());
        let outcome = request.map(|req| super::ureq::run(req).is_err());
        assert_eq!(outcome, Ok(true));
    }

    #[test]
    fn the_agent_uses_the_provider() {
        let agent = http_agent();
        let configured = agent
            .config()
            .tls_config()
            .unversioned_rustls_crypto_provider()
            .as_ref()
            .map(Arc::clone);
        assert!(configured.is_some_and(|p| Arc::ptr_eq(&p, &crypto_provider())));
    }
}
