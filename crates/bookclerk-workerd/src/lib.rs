//! Library surface for the pinned Cloudflare `workerd` helper and egress policy.
//!
//! # Audience
//!
//! The `bookclerk-workerd` launcher binary and host packaging (`cargo ensure-workerd`).
//! Guest authors do not link this crate; they run inside the isolate it configures.
//!
//! See `docs/plugins.md` for jail / workerd isolation requirements.

pub mod bridge_http;
pub mod bridge_stdio;
pub mod config;
pub mod egress;
pub mod ensure;
pub mod grant;
pub mod granted;
pub mod native_broker;
pub mod notify;
pub mod pin;

pub use config::{
    adapter_binding_plan, generated_backend_proxy_plan, materialize_native_backend, BindingSpec,
    BindingTarget, EntrypointSource, GeneratedConfig, ListenSpec,
};
pub use egress::EgressProxy;
pub use ensure::{ensure_workerd, workerd_bin_path};
pub use grant::OperatorGrantEnv;
pub use pin::{
    binary_name, BUNDLED_WORKERD_COMPAT_DATE, WORKERD_RELEASE_TAG, WORKERD_VERSION_STAMP,
};
