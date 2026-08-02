//! Windows backend.
//!
//! Windows has no self-confinement primitive: a process cannot drop itself into
//! an AppContainer after it has started. Isolation is granted at process
//! creation, by giving the child a token with an AppContainer SID and ACLing the
//! paths it may reach.
//!
//! So [`confine_current_process`] reports the filesystem layer as
//! [`LayerStatus::Unsupported`] here, which under [`crate::Enforcement::Required`]
//! is an error for self-confine callers (media workers). Plugin guests are
//! confined at spawn via [`super::windows_spawn::run_appcontainer`].

use crate::{Capabilities, LayerStatus, NetPolicy, Policy, Report, SandboxError};

/// Backend name reported in diagnostics.
pub const BACKEND: &str = "appcontainer";

/// Read-only paths a Windows process needs to load.
///
/// Unlike Unix, these are not applied as an allowlist by
/// [`confine_current_process`]. On Windows this stays empty on purpose:
/// Bookclerk must not put `System32` (or other OS trees) on the AppContainer
/// allowlist or try to ACE them. Guests load system binaries through the OS
/// ALL APPLICATION PACKAGES defaults; confinement is the explicit ACL grants
/// for plugin/user paths only. See [`super::windows_spawn`].
pub fn system_read_paths() -> &'static [&'static str] {
    &[]
}

/// No self-confinement here, so there is no system set to widen.
pub fn system_write_paths() -> &'static [&'static str] {
    &[]
}

pub fn capabilities() -> Capabilities {
    Capabilities {
        backend: BACKEND,
        // True for children spawned into an AppContainer, not for self-confinement.
        filesystem: false,
        spawn_filesystem: true,
        syscall: false,
        network: true,
        detail: "AppContainer at CreateProcess (no self-confinement); \
                 plugin guests are confined by bookclerk-jail via \
                 windows_spawn::run_appcontainer"
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
            NetPolicy::Deny | NetPolicy::Outbound | NetPolicy::OutboundListen => unsupported,
        },
    })
}
