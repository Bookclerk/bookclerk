//! Library surface for the pinned Cloudflare `workerd` helper and egress policy.

pub mod config;
pub mod egress;
pub mod ensure;
pub mod notify;
pub mod pin;

pub use egress::EgressProxy;
pub use ensure::{ensure_workerd, workerd_bin_path};
pub use pin::{
    binary_name, BUNDLED_WORKERD_COMPAT_DATE, WORKERD_RELEASE_TAG, WORKERD_VERSION_STAMP,
};
