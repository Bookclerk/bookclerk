//! Library surface for the pinned Cloudflare `workerd` helper and egress policy.
//!
//! # Audience
//!
//! The `bookclerk-workerd` launcher binary and host packaging (`cargo ensure-workerd`).
//! Guest authors do not link this crate; they run inside the isolate it configures.
//!
//! See `docs/plugins/README.md` for jail / workerd isolation requirements.

// Pre-existing sites, grandfathered until fixed: https://github.com/Bookclerk/bookclerk/issues/284
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::expect_used,
    clippy::unwrap_used
)]

pub mod bridge_addr;
pub mod bridge_http;
pub mod bridge_stdio;
pub mod config;
pub mod control;
pub mod egress;
pub mod egress_relay;
pub mod ensure;
pub mod grant;
pub mod granted;
pub mod granted_http;
pub mod granted_sockets;
pub mod granted_webhooks;
pub mod inherited_link;
pub mod invoke;
pub mod pin;
pub mod python_snapshot;
pub mod socket_broker;
pub mod socket_proxy;
pub mod socket_table;
pub mod state_sweep;
#[cfg(unix)]
pub mod unix_bind;
pub mod webhook_request;

pub use bridge_http::generate_bridge_token;
pub use config::{
    adapter_binding_plan, generated_backend_proxy_plan, materialize_native_backend, BindingSpec,
    BindingTarget, EntrypointSource, GeneratedConfig, ListenSpec, WORKERD_SERVE_EXPERIMENTAL,
};
pub use egress::EgressProxy;
pub use ensure::{ensure_pyodide_bundle, ensure_workerd, pyodide_bundle_dir, workerd_bin_path};
pub use grant::OperatorGrantEnv;
pub use pin::{
    binary_name, pyodide_bundle_url, BUNDLED_WORKERD_COMPAT_DATE, PYODIDE_BUNDLE_DIR,
    PYODIDE_BUNDLE_FILE, PYODIDE_BUNDLE_SHA256_HEX, WORKERD_RELEASE_TAG, WORKERD_VERSION_STAMP,
};
