//! Windows backend.
//!
//! Windows has no self-confinement primitive: a process cannot drop itself into
//! an AppContainer after it has started. Isolation is granted at process
//! creation, by giving the child a token with an AppContainer SID and ACLing the
//! paths it may reach.
//!
//! So [`confine_current_process`] reports the filesystem layer as
//! [`LayerStatus::Unsupported`] here, which under [`crate::Enforcement::Required`]
//! is an error — callers on Windows must confine children at spawn instead.
//! The spawn-side implementation lands with the plugin and media-worker jails.

use crate::{Capabilities, LayerStatus, NetPolicy, Policy, Report, SandboxError};

/// Backend name reported in diagnostics.
pub const BACKEND: &str = "appcontainer";

/// Read-only paths a Windows process needs to load.
///
/// Unlike Unix, these are not applied as an allowlist by
/// [`confine_current_process`]; they describe what a spawn-side AppContainer
/// profile must grant.
pub fn system_read_paths() -> &'static [&'static str] {
    &[]
}

pub fn capabilities() -> Capabilities {
    Capabilities {
        backend: BACKEND,
        // True for children spawned into an AppContainer, not for self-confinement.
        filesystem: false,
        syscall: false,
        network: false,
        detail: "AppContainer applies at process creation; Windows has no \
                 self-confinement primitive"
            .to_string(),
    }
}

pub fn confine_current_process(policy: &Policy) -> Result<Report, SandboxError> {
    let unsupported = LayerStatus::Unsupported(
        "Windows cannot confine a running process; spawn into an AppContainer instead".to_string(),
    );
    Ok(Report {
        label: policy.label().to_string(),
        backend: BACKEND,
        filesystem: unsupported.clone(),
        syscall: LayerStatus::Unsupported("no syscall filtering on Windows".to_string()),
        network: match policy.net_policy() {
            NetPolicy::Full => LayerStatus::NotRequested,
            NetPolicy::Deny | NetPolicy::Outbound => unsupported,
        },
    })
}
