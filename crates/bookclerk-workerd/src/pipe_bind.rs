//! Named-pipe SOCKET_PROXY for native-behind-workerd guests on Windows.
//!
//! Nested AppContainer (`NetPolicy::Deny`) cannot open `AF_INET`. The launcher
//! creates a local pipe whose DACL includes the nested Package SID (or
//! [`ALL_APPLICATION_PACKAGES_SID`] when the SID is not known at bind time).
//! The native SDK CONNECTs through that pipe; the same [`EgressPolicy`] as
//! workerd `fetch()`/`connect()` is applied in [`crate::socket_proxy`].

#![cfg(windows)]
#![allow(clippy::missing_docs_in_private_items)]

use std::io;

use rand::RngCore;
use tokio::net::windows::named_pipe::{NamedPipeServer, PipeMode, ServerOptions};

/// Well-known SID `APPLICATION PACKAGE AUTHORITY\ALL APPLICATION PACKAGES`.
///
/// Used when the host could not pre-create the nested AppContainer profile so
/// the jail will mint a unique SID at spawn. Combined with an unguessable pipe
/// name this is still local-only (`reject_remote_clients`).
pub const ALL_APPLICATION_PACKAGES_SID: &str = "S-1-15-2-1";

/// Bound native TCP proxy advertised as `BOOKCLERK_SOCKET_PROXY`.
pub struct BoundSocketProxy {
    /// Value for [`crate::socket_proxy::SOCKET_PROXY_ENV`].
    pub spec: String,
    /// Pipe name (`\\.\pipe\bc-s-…`) for subsequent instances.
    pub name: String,
    /// Package SID embedded in each instance's DACL.
    pub package_sid: String,
    /// First pipe instance; must exist before the nested guest is spawned.
    pub first: NamedPipeServer,
}

/// Allocates a pipe name and creates the first instance.
///
/// # Errors
///
/// Returns when Win32 `CreateNamedPipe` fails or the Package SID SDDL is
/// rejected.
pub fn bind_socket_proxy(package_sid: Option<&str>) -> io::Result<BoundSocketProxy> {
    let mut nonce = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut nonce);
    let name = format!(r"\\.\pipe\bc-s-{}", hex::encode(nonce));
    let package_sid = package_sid
        .filter(|sid| !sid.is_empty())
        .unwrap_or(ALL_APPLICATION_PACKAGES_SID)
        .to_string();
    let first = create_pipe(&name, &package_sid, true)?;
    Ok(BoundSocketProxy {
        spec: name.clone(),
        name,
        package_sid,
        first,
    })
}

/// Creates one named-pipe server instance with an AppContainer-capable DACL.
///
/// # Errors
///
/// Returns when the security descriptor cannot be built or `CreateNamedPipe`
/// fails.
#[allow(unsafe_code)]
pub fn create_pipe(name: &str, package_sid: &str, first: bool) -> io::Result<NamedPipeServer> {
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first)
        .reject_remote_clients(true)
        .pipe_mode(PipeMode::Byte);
    let mut sec = bookclerk_sandbox::spawn::NamedPipeSecurity::for_app_container(package_sid)
        .map_err(|err| io::Error::other(err.to_string()))?;
    // SAFETY: `sec` owns a valid SECURITY_ATTRIBUTES until this call returns;
    // CreateNamedPipe copies the descriptor onto the pipe object.
    unsafe { options.create_with_security_attributes_raw(name, sec.as_mut_ptr()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pipe_name_is_local_and_unguessable() {
        let a = bind_socket_proxy(Some(ALL_APPLICATION_PACKAGES_SID)).expect("pipe a");
        let b = bind_socket_proxy(None).expect("pipe b");
        assert!(a.spec.starts_with(r"\\.\pipe\bc-s-"), "{}", a.spec);
        assert_ne!(a.spec, b.spec);
        assert_eq!(a.package_sid, ALL_APPLICATION_PACKAGES_SID);
        assert_eq!(b.package_sid, ALL_APPLICATION_PACKAGES_SID);
        drop(a.first);
        drop(b.first);
    }
}
