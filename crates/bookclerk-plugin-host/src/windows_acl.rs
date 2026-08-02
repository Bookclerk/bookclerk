//! Per-operation filesystem grants for Windows guests (AppContainer ACL).
//!
//! Unix guests receive open directory/file descriptors over `SCM_RIGHTS` on
//! fd 3. Windows has no equivalent side channel once the process is running in
//! an AppContainer; instead the host temporarily ACLs the target path for the
//! guest's Package SID, passes the path on the JSON-RPC wire, then removes the
//! ACE after the call returns.

#![cfg(windows)]

use std::path::Path;

use bookclerk_sandbox::spawn::AclGrant;

use crate::{PluginError, Result};

/// Grant the AppContainer Package SID access to `path` for the duration of one
/// RPC (fetch dir / upload file / sqlite db).
///
/// # Errors
///
/// Returns an error when the Win32 ACL APIs refuse the grant.
pub fn grant_path_for_guest(package_sid: &str, path: &Path, write: bool) -> Result<AclGuard> {
    let grant =
        bookclerk_sandbox::spawn::grant_path_access(package_sid, path, write).map_err(|err| {
            PluginError::message(format!(
                "could not ACL-grant {} for AppContainer guest: {err}",
                path.display()
            ))
        })?;
    Ok(AclGuard { inner: Some(grant) })
}

/// RAII guard that revokes the temporary ACE on drop.
pub struct AclGuard {
    inner: Option<AclGrant>,
}

impl Drop for AclGuard {
    fn drop(&mut self) {
        // `AclGrant::drop` performs the revoke.
        self.inner.take();
    }
}
