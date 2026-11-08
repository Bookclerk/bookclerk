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
//! - [`http_agent`] is the blocking HTTP client (`ureq`) every host crate uses
//!   instead of `ureq::get` / `ureq::post`, whose default agent would need a
//!   provider of its own.
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
