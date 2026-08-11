//! Fallback for targets with no confinement backend.
//!
//! Reports every layer as unsupported so [`crate::Enforcement::Required`] fails
//! closed rather than silently running unconfined.

use crate::{Capabilities, LayerStatus, Policy, Report, SandboxError};

/// Backend name reported in diagnostics.
pub const BACKEND: &str = "none";

pub fn system_read_paths() -> &'static [&'static str] {
    &[]
}

pub fn system_write_paths() -> &'static [&'static str] {
    &[]
}

pub fn capabilities() -> Capabilities {
    Capabilities {
        backend: BACKEND,
        filesystem: false,
        spawn_filesystem: false,
        syscall: false,
        network: false,
        detail: format!("no confinement backend for {}", std::env::consts::OS),
    }
}

pub fn confine_current_process(policy: &Policy) -> Result<Report, SandboxError> {
    let detail = format!("no confinement backend for {}", std::env::consts::OS);
    Ok(Report {
        label: policy.label().to_string(),
        backend: BACKEND,
        filesystem: LayerStatus::Unsupported(detail.clone()),
        syscall: LayerStatus::Unsupported(detail.clone()),
        network: LayerStatus::Unsupported(detail.clone()),
        resources: if policy.has_resource_limits() {
            LayerStatus::Unsupported(detail)
        } else {
            LayerStatus::NotRequested
        },
    })
}
