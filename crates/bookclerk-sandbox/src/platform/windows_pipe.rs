//! Named-pipe security descriptors for AppContainer guests.
//!
//! Default `CreateNamedPipe` DACLs do not grant the guest Package SID, so an
//! AppContainer client gets `ERROR_ACCESS_DENIED` even when the pipe name is
//! known. AppContainers also run at Low integrity; without a Low mandatory
//! label on the object, the integrity check fails independently of the DACL.
//!
//! This module builds a short-lived [`SECURITY_ATTRIBUTES`] for
//! `CreateNamedPipe` / Tokio's `create_with_security_attributes_raw`:
//! - DACL: SYSTEM + Administrators + Creator Owner (full) + Package SID (R/W)
//! - SACL: Low mandatory label (`S:(ML;;NW;;;LW)`)

#![cfg_attr(windows, allow(unsafe_code))]

use crate::SandboxError;

/// Owned `SECURITY_ATTRIBUTES` suitable for `CreateNamedPipe` to one guest.
///
/// Keep this value alive until the create call returns; the kernel copies the
/// descriptor onto the pipe object.
pub struct NamedPipeSecurity {
    #[cfg(windows)]
    /// Win32 `SECURITY_ATTRIBUTES` backing [`Self::as_mut_ptr`].
    attrs: windows::Win32::Security::SECURITY_ATTRIBUTES,
}

impl NamedPipeSecurity {
    /// Build attributes that allow `package_sid` to open a duplex named pipe.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::Backend`] when `package_sid` is not a Package
    /// SID string, or when Win32 refuses to parse the SDDL.
    pub fn for_app_container(package_sid: &str) -> Result<Self, SandboxError> {
        validate_package_sid(package_sid)?;
        let sddl = sddl_for_package(package_sid);
        Self::from_sddl(&sddl)
    }

    /// Raw pointer for Tokio `create_with_security_attributes_raw` / Win32.
    #[must_use]
    pub fn as_mut_ptr(&mut self) -> *mut std::ffi::c_void {
        #[cfg(windows)]
        {
            (&mut self.attrs as *mut windows::Win32::Security::SECURITY_ATTRIBUTES).cast()
        }
        #[cfg(not(windows))]
        {
            std::ptr::null_mut()
        }
    }

    /// Builds pipe security attributes from an SDDL string.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::Backend`] when Win32 rejects the SDDL.
    #[cfg(windows)]
    fn from_sddl(sddl: &str) -> Result<Self, SandboxError> {
        use std::mem::size_of;

        use windows::core::PCWSTR;
        use windows::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

        let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut psd = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
        unsafe {
            if let Err(err) = ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wide.as_ptr()),
                SDDL_REVISION_1,
                &mut psd,
                None,
            ) {
                return Err(SandboxError::Backend {
                    label: "appcontainer".into(),
                    backend: "appcontainer",
                    detail: format!(
                        "ConvertStringSecurityDescriptorToSecurityDescriptorW failed: {err}"
                    ),
                });
            }
        }
        if psd.0.is_null() {
            return Err(SandboxError::Backend {
                label: "appcontainer".into(),
                backend: "appcontainer",
                detail: "security descriptor conversion returned null".into(),
            });
        }

        Ok(Self {
            attrs: SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: psd.0,
                bInheritHandle: false.into(),
            },
        })
    }

    #[cfg(not(windows))]
    /// Non-Windows stub: named-pipe security descriptors are unavailable and always fail.
    fn from_sddl(_sddl: &str) -> Result<Self, SandboxError> {
        Err(SandboxError::Backend {
            label: "appcontainer".into(),
            backend: "appcontainer",
            detail: "named-pipe security descriptors require Windows".into(),
        })
    }
}

impl Drop for NamedPipeSecurity {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            use windows::Win32::Foundation::{LocalFree, HLOCAL};
            let ptr = self.attrs.lpSecurityDescriptor;
            if !ptr.is_null() {
                self.attrs.lpSecurityDescriptor = std::ptr::null_mut();
                unsafe {
                    let _ = LocalFree(Some(HLOCAL(ptr)));
                }
            }
        }
    }
}

/// AppContainer Package SIDs are `S-1-15-2-…` (capability SIDs use `S-1-15-3-`).
fn validate_package_sid(sid: &str) -> Result<(), SandboxError> {
    if !sid.starts_with("S-1-15-2-") {
        return Err(SandboxError::Backend {
            label: "appcontainer".into(),
            backend: "appcontainer",
            detail: format!("expected AppContainer Package SID (S-1-15-2-…), got {sid:?}"),
        });
    }
    // Digits and hyphens only after the leading `S` — keeps SDDL interpolation safe.
    let rest = &sid[1..];
    if rest.is_empty()
        || !rest.chars().all(|c| c.is_ascii_digit() || c == '-')
        || rest.contains("--")
        || rest.ends_with('-')
    {
        return Err(SandboxError::Backend {
            label: "appcontainer".into(),
            backend: "appcontainer",
            detail: format!("malformed Package SID: {sid:?}"),
        });
    }
    Ok(())
}

/// Builds the DACL/SACL SDDL that grants a Package SID duplex access at Low integrity.
fn sddl_for_package(package_sid: &str) -> String {
    // FA = FILE_ALL_ACCESS for host/system trustees; GRGW = duplex client open.
    // ML;;NW;;;LW = Low integrity label so Low-IL AppContainer clients pass MIC.
    format!("D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;OW)(A;;GRGW;;;{package_sid})S:(ML;;NW;;;LW)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_package_sid_shape() {
        validate_package_sid("S-1-15-2-1-2-3-4-5-6-7-8").unwrap();
    }

    #[test]
    fn rejects_capability_sid() {
        assert!(validate_package_sid("S-1-15-3-1").is_err());
    }

    #[test]
    fn rejects_sddl_metacharacters() {
        assert!(validate_package_sid("S-1-15-2-1)(A;;GA;;;WD").is_err());
        assert!(validate_package_sid("S-1-15-2-1;GA").is_err());
    }

    #[test]
    fn sddl_embeds_sid_and_low_label() {
        let sid = "S-1-15-2-99-88-77";
        let sddl = sddl_for_package(sid);
        assert!(sddl.contains(sid));
        assert!(sddl.contains("S:(ML;;NW;;;LW)"));
        assert!(sddl.contains("(A;;GRGW;;;"));
        assert!(sddl.starts_with("D:P"));
    }
}
