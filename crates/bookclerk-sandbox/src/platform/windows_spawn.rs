//! Windows AppContainer spawn.
//!
//! Windows cannot confine a process after it has started. Isolation is granted
//! at `CreateProcess` by giving the child a token with an AppContainer SID and
//! named capability SIDs, then ACLing the paths it may reach.

#![cfg_attr(windows, allow(unsafe_code))] // Win32 ACL revoke uses raw SID/ACL APIs.
//!
//! [`plan_appcontainer`] maps a [`Policy`] to capability names (and, on Windows,
//! ensures a profile so the Package SID is known). [`run_appcontainer`] creates
//! the profile, grants ACLs for the policy's allowlist, launches the guest
//! inside the container, and proxies stdio until it exits — that is what
//! `bookclerk-jail` uses on Windows instead of self-confine + `exec`.

use std::ffi::OsString;
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;

use crate::{NetPolicy, Policy, SandboxError};

/// Planned AppContainer launch parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppContainerLaunch {
    /// AppContainer profile moniker (sanitized from the policy label).
    pub profile_name: String,
    /// Package SID as an SDDL string once the profile exists (Windows only).
    pub package_sid: Option<String>,
    /// Named capability SIDs to grant (well-known capability names).
    pub capability_names: Vec<&'static str>,
}

/// Map a confinement [`Policy`] into AppContainer network capabilities.
///
/// | [`NetPolicy`] | Capabilities |
/// | --- | --- |
/// | [`Deny`](NetPolicy::Deny) | _(none)_ |
/// | [`Outbound`](NetPolicy::Outbound) | `internetClient` |
/// | [`OutboundListen`](NetPolicy::OutboundListen) | `internetClient`, `privateNetworkClientServer` |
/// | [`Full`](NetPolicy::Full) | `internetClient`, `internetClientServer`, `privateNetworkClientServer` |
///
/// On Windows this also ensures the AppContainer profile so [`AppContainerLaunch::package_sid`]
/// is populated.
pub fn plan_appcontainer(policy: &Policy) -> Result<AppContainerLaunch, SandboxError> {
    let capability_names = capability_names_for(policy.net_policy());
    let profile_name = profile_name_for_label(policy.label());
    let package_sid = ensure_package_sid(&profile_name, policy)?;
    Ok(AppContainerLaunch {
        profile_name,
        package_sid,
        capability_names,
    })
}

/// Sanitize a policy label into a valid AppContainer profile moniker.
///
/// Names may contain only alphanumeric characters and `.`, and must be ≤ 64
/// characters (Win32 `CreateAppContainerProfile` constraints).
#[must_use]
pub fn profile_name_for_label(label: &str) -> String {
    let mut out = String::from("bookclerk.");
    for ch in label.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if ch == '.' || ch == '-' || ch == '_' || ch == ':' {
            out.push('.');
        }
    }
    while out.contains("..") {
        out = out.replace("..", ".");
    }
    let trimmed = out.trim_matches('.').to_string();
    // Only the fixed prefix means the label contributed nothing usable.
    let mut name = if trimmed.is_empty() || trimmed == "bookclerk" {
        "bookclerk.guest".to_string()
    } else {
        trimmed
    };
    // Win32 limit is 64 characters.
    if name.len() > 64 {
        name.truncate(64);
        name = name.trim_end_matches('.').to_string();
    }
    name
}

/// Resolve (creating if needed) the Package SID SDDL for a policy label.
pub fn package_sid_for_label(label: &str) -> Result<String, SandboxError> {
    let name = profile_name_for_label(label);
    ensure_package_sid(&name, &Policy::new(label))?.ok_or_else(|| SandboxError::Backend {
        label: label.to_string(),
        backend: "appcontainer",
        detail: "package SID unavailable on this platform".to_string(),
    })
}

fn capability_names_for(net: NetPolicy) -> Vec<&'static str> {
    match net {
        NetPolicy::Deny => Vec::new(),
        NetPolicy::Outbound => vec!["internetClient"],
        NetPolicy::OutboundListen => vec!["internetClient", "privateNetworkClientServer"],
        NetPolicy::Full => vec![
            "internetClient",
            "internetClientServer",
            "privateNetworkClientServer",
        ],
    }
}

#[cfg(windows)]
fn ensure_package_sid(profile_name: &str, policy: &Policy) -> Result<Option<String>, SandboxError> {
    use rappct::AppContainerProfile;

    let profile = AppContainerProfile::ensure(
        profile_name,
        &format!("Bookclerk {}", policy.label()),
        Some("Bookclerk plugin / media guest AppContainer"),
    )
    .map_err(|err| SandboxError::Backend {
        label: policy.label().to_string(),
        backend: "appcontainer",
        detail: format!("CreateAppContainerProfile failed: {err}"),
    })?;
    Ok(Some(profile.sid.as_string().to_string()))
}

#[cfg(not(windows))]
fn ensure_package_sid(
    _profile_name: &str,
    _policy: &Policy,
) -> Result<Option<String>, SandboxError> {
    Ok(None)
}

/// RAII guard that revokes a temporary Package-SID ACE on drop.
#[derive(Debug)]
pub struct AclGrant {
    #[cfg(windows)]
    path: std::path::PathBuf,
    #[cfg(windows)]
    package_sid: String,
    #[cfg(windows)]
    is_dir: bool,
}

impl AclGrant {
    /// Path this grant covers.
    #[must_use]
    pub fn path(&self) -> &Path {
        #[cfg(windows)]
        {
            &self.path
        }
        #[cfg(not(windows))]
        {
            Path::new("")
        }
    }
}

impl Drop for AclGrant {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            if let Err(err) = revoke_package_access(&self.path, &self.package_sid, self.is_dir) {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %err,
                    "failed to revoke AppContainer ACL grant"
                );
            }
        }
    }
}

/// Grant the Package SID access to `path` for one RPC / spawn allowlist entry.
///
/// When `write` is true the ACE includes generic write; otherwise read+execute.
/// The returned [`AclGrant`] revokes the ACE on drop.
pub fn grant_path_access(
    package_sid: &str,
    path: &Path,
    write: bool,
) -> Result<AclGrant, SandboxError> {
    #[cfg(windows)]
    {
        grant_package_access(package_sid, path, write)?;
        Ok(AclGrant {
            path: path.to_path_buf(),
            package_sid: package_sid.to_string(),
            is_dir: path.is_dir(),
        })
    }
    #[cfg(not(windows))]
    {
        let _ = (package_sid, path, write);
        Err(SandboxError::Backend {
            label: "appcontainer".to_string(),
            backend: "appcontainer",
            detail: "ACL grants require Windows".to_string(),
        })
    }
}

/// Spawn `program` inside an AppContainer derived from `policy`, proxy stdio,
/// and return the child's exit code.
///
/// Stdio is proxied (not inherited) because Win32 AppContainer launch via
/// `CreateProcess` with extended attributes does not reliably inherit the
/// launcher's redirected pipes; the jail process sits in the middle and copies
/// bytes until the guest exits.
///
/// # Errors
///
/// Returns [`SandboxError::Backend`] when the profile, ACLs, or CreateProcess
/// step fails. On non-Windows hosts this always fails.
pub fn run_appcontainer(
    policy: &Policy,
    program: &Path,
    args: &[OsString],
) -> Result<u32, SandboxError> {
    #[cfg(windows)]
    {
        run_appcontainer_windows(policy, program, args)
    }
    #[cfg(not(windows))]
    {
        let _ = (program, args);
        let _ = plan_appcontainer(policy)?;
        Err(SandboxError::Backend {
            label: policy.label().to_string(),
            backend: "appcontainer",
            detail: "AppContainer CreateProcess is only available on Windows".to_string(),
        })
    }
}

#[cfg(windows)]
fn run_appcontainer_windows(
    policy: &Policy,
    program: &Path,
    args: &[OsString],
) -> Result<u32, SandboxError> {
    use std::io::{self, Write};
    use std::thread;

    use rappct::acl::{grant_to_package, AccessMask, ResourcePath};
    use rappct::launch::{launch_in_container_with_io, LaunchOptions, StdioConfig};
    use rappct::{AppContainerProfile, SecurityCapabilitiesBuilder};

    let plan = plan_appcontainer(policy)?;
    let package_sid = plan
        .package_sid
        .clone()
        .ok_or_else(|| SandboxError::Backend {
            label: policy.label().to_string(),
            backend: "appcontainer",
            detail: "missing package SID after profile ensure".to_string(),
        })?;

    let profile = AppContainerProfile::ensure(
        &plan.profile_name,
        &format!("Bookclerk {}", policy.label()),
        Some("Bookclerk plugin / media guest AppContainer"),
    )
    .map_err(|err| SandboxError::Backend {
        label: policy.label().to_string(),
        backend: "appcontainer",
        detail: format!("CreateAppContainerProfile failed: {err}"),
    })?;

    let mut builder = SecurityCapabilitiesBuilder::new(&profile.sid);
    if !plan.capability_names.is_empty() {
        builder = builder.with_named(&plan.capability_names);
    }
    let sec = builder.build().map_err(|err| SandboxError::Backend {
        label: policy.label().to_string(),
        backend: "appcontainer",
        detail: format!("capability SID derivation failed: {err}"),
    })?;

    // Keep grants for the life of the launch; revoke afterward.
    let mut grants: Vec<AclGrant> = Vec::new();
    let mut allowlisted: Vec<PathBuf> = Vec::new();
    for path in policy.resolved_reads() {
        // OS trees (System32, Program Files, …) reject WRITE_DAC for normal
        // users. AppContainers already receive read/exec there via the OS
        // ALL APPLICATION PACKAGES grants, so skipping is required — not a hole.
        if is_os_protected_path(&path) {
            tracing::debug!(
                path = %path.display(),
                "skipping AppContainer ACL grant on OS-protected path"
            );
            continue;
        }
        let access = AccessMask(AccessMask::FILE_GENERIC_READ.0 | FILE_GENERIC_EXECUTE);
        let target = if path.is_dir() {
            ResourcePath::Directory(path.clone())
        } else {
            ResourcePath::File(path.clone())
        };
        let is_dir = path.is_dir();
        grant_to_package(target, &profile.sid, access).map_err(|err| SandboxError::Backend {
            label: policy.label().to_string(),
            backend: "appcontainer",
            detail: format!("ACL grant (read) {}: {err}", path.display()),
        })?;
        allowlisted.push(path.clone());
        grants.push(AclGrant {
            path,
            package_sid: package_sid.clone(),
            is_dir,
        });
    }
    for path in policy.resolved_writes() {
        let access = AccessMask(
            AccessMask::FILE_GENERIC_READ.0
                | AccessMask::FILE_GENERIC_WRITE.0
                | FILE_GENERIC_EXECUTE,
        );
        let target = if path.is_dir() {
            ResourcePath::Directory(path.clone())
        } else {
            ResourcePath::File(path.clone())
        };
        grant_to_package(target, &profile.sid, access).map_err(|err| SandboxError::Backend {
            label: policy.label().to_string(),
            backend: "appcontainer",
            detail: format!("ACL grant (write) {}: {err}", path.display()),
        })?;
        allowlisted.push(path.clone());
        grants.push(AclGrant {
            path: path.clone(),
            package_sid: package_sid.clone(),
            is_dir: path.is_dir(),
        });
    }

    // AppContainers cannot walk into a granted leaf without FILE_TRAVERSE on
    // each ancestor. rappct's directory grant inherits onto children, so we
    // must NOT grant `%TEMP%` itself (that would open sibling "forbidden"
    // trees). Instead grant each ancestor with no-inheritance traverse only.
    let mut seen_ancestors = std::collections::HashSet::new();
    for path in &allowlisted {
        for ancestor in ancestor_directories(path) {
            if !seen_ancestors.insert(ancestor.clone()) {
                continue;
            }
            if is_os_protected_path(&ancestor) {
                continue;
            }
            match grant_directory_traverse_no_inherit(&package_sid, &ancestor) {
                Ok(()) => grants.push(AclGrant {
                    path: ancestor,
                    package_sid: package_sid.clone(),
                    is_dir: true,
                }),
                Err(err) => {
                    tracing::debug!(
                        path = %ancestor.display(),
                        error = %err,
                        "optional ancestor traverse ACL grant failed"
                    );
                }
            }
        }
    }

    // Also grant the executable path when it was not already covered (absolute
    // command outside the install root). Soft-fail: System32 rejects WRITE_DAC.
    if program.exists() && !is_os_protected_path(program) {
        let access = AccessMask(AccessMask::FILE_GENERIC_READ.0 | FILE_GENERIC_EXECUTE);
        let target = ResourcePath::File(program.to_path_buf());
        if let Err(err) = grant_to_package(target, &profile.sid, access) {
            tracing::debug!(
                path = %program.display(),
                error = %err,
                "optional ACL grant for guest executable failed"
            );
        } else {
            grants.push(AclGrant {
                path: program.to_path_buf(),
                package_sid: package_sid.clone(),
                is_dir: false,
            });
        }
    }

    // Match rappct's CreateProcess convention: lpApplicationName = exe,
    // lpCommandLine = args only (starting at /C …). Cwd must be an
    // AppContainer-readable directory (System32 via OS defaults).
    let cmdline = windows_args_command_line(args);
    let opts = LaunchOptions {
        exe: program.to_path_buf(),
        cmdline: Some(cmdline),
        cwd: Some(PathBuf::from(r"C:\Windows\System32")),
        env: None,
        stdio: StdioConfig::Pipe,
        suspended: false,
        join_job: None,
        startup_timeout: None,
    };

    let mut io = launch_in_container_with_io(&sec, &opts).map_err(|err| SandboxError::Backend {
        label: policy.label().to_string(),
        backend: "appcontainer",
        detail: format!("CreateProcess AppContainer failed: {err}"),
    })?;

    let mut child_stdin = io.stdin.take();
    let mut child_stdout = io.stdout.take();
    let mut child_stderr = io.stderr.take();

    let t_in = thread::spawn(move || {
        if let Some(mut dest) = child_stdin.take() {
            let _ = io::copy(&mut io::stdin(), &mut dest);
            let _ = dest.flush();
        }
    });
    let t_out = thread::spawn(move || {
        if let Some(mut src) = child_stdout.take() {
            let _ = io::copy(&mut src, &mut io::stdout());
            let _ = io::stdout().flush();
        }
    });
    let t_err = thread::spawn(move || {
        if let Some(mut src) = child_stderr.take() {
            let _ = io::copy(&mut src, &mut io::stderr());
            let _ = io::stderr().flush();
        }
    });

    let code = io.wait(None).map_err(|err| SandboxError::Backend {
        label: policy.label().to_string(),
        backend: "appcontainer",
        detail: format!("waiting for AppContainer guest failed: {err}"),
    })?;

    let _ = t_in.join();
    let _ = t_out.join();
    let _ = t_err.join();

    // Explicit drop order: revoke ACLs after the guest has exited.
    drop(grants);

    tracing::debug!(
        label = %policy.label(),
        profile = %plan.profile_name,
        package_sid = %package_sid,
        exit = code,
        "AppContainer guest exited"
    );
    Ok(code)
}

/// `FILE_GENERIC_EXECUTE` (not always re-exported by every windows-rs feature set).
#[cfg(windows)]
const FILE_GENERIC_EXECUTE: u32 = 0x0012_00A0;

/// Build CreateProcess `lpCommandLine` as args only (exe is `lpApplicationName`).
///
/// The argument immediately after `cmd`'s `/C` or `/K` is joined **raw**: wrapping
/// a multi-word script in quotes triggers cmd's quote rule and breaks `&&`
/// chains (the failure mode behind "Access is denied" on inline test scripts).
/// Embedded paths inside that script must already be quoted by the caller.
#[cfg(windows)]
fn windows_args_command_line(args: &[OsString]) -> String {
    let mut line = String::new();
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            line.push(' ');
        }
        let prev_is_cmd_script = i > 0
            && matches!(
                args[i - 1].to_string_lossy().as_ref(),
                "/C" | "/c" | "/K" | "/k"
            );
        if prev_is_cmd_script {
            line.push_str(&arg.to_string_lossy());
        } else {
            line.push_str(&quote_windows_arg(arg));
        }
    }
    line
}

/// Parent directories of `path` up to (but not including) the drive root.
#[cfg(windows)]
fn ancestor_directories(path: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut cur = path;
    while let Some(parent) = cur.parent() {
        if parent.as_os_str().is_empty() {
            break;
        }
        let text = parent.to_string_lossy();
        // `C:\` / `C:` — stop; do not ACE the volume root.
        let trimmed = text.trim_end_matches(['\\', '/']);
        if trimmed.len() == 2 && trimmed.as_bytes().get(1) == Some(&b':') {
            break;
        }
        out.push(parent.to_path_buf());
        cur = parent;
    }
    out
}

/// Grant traverse/list on a directory with **no** inheritance.
///
/// Unlike [`grant_to_package`] on a directory (which inherits onto children),
/// this only opens the directory itself so the guest can walk to an allowlisted
/// leaf without unlocking sibling trees under a shared parent like `%TEMP%`.
#[cfg(windows)]
fn grant_directory_traverse_no_inherit(package_sid: &str, path: &Path) -> Result<(), SandboxError> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertStringSidToSidW, GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW,
        EXPLICIT_ACCESS_W, GRANT_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_SID,
        TRUSTEE_IS_WELL_KNOWN_GROUP, TRUSTEE_W,
    };
    use windows::Win32::Security::{ACE_FLAGS, ACL, DACL_SECURITY_INFORMATION, PSID};
    use windows::Win32::Storage::FileSystem::FILE_GENERIC_READ;

    let path_w: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let sid_w: Vec<u16> = package_sid
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let mut psid = PSID(ptr::null_mut());
        ConvertStringSidToSidW(PCWSTR(sid_w.as_ptr()), &mut psid).map_err(|err| {
            SandboxError::Backend {
                label: "appcontainer".into(),
                backend: "appcontainer",
                detail: format!("ConvertStringSidToSidW failed: {err}"),
            }
        })?;

        let mut trustee: TRUSTEE_W = std::mem::zeroed();
        trustee.TrusteeForm = TRUSTEE_IS_SID;
        trustee.TrusteeType = TRUSTEE_IS_WELL_KNOWN_GROUP;
        trustee.ptstrName = PWSTR(psid.0.cast());

        let mut ea: EXPLICIT_ACCESS_W = std::mem::zeroed();
        ea.grfAccessPermissions = FILE_GENERIC_READ.0 | FILE_GENERIC_EXECUTE;
        ea.grfAccessMode = GRANT_ACCESS;
        ea.grfInheritance = ACE_FLAGS(0); // this directory only
        ea.Trustee = trustee;

        let mut p_sd = windows::Win32::Security::PSECURITY_DESCRIPTOR(ptr::null_mut());
        let mut p_dacl: *mut ACL = ptr::null_mut();
        let st = GetNamedSecurityInfoW(
            PCWSTR(path_w.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut p_dacl),
            None,
            &mut p_sd,
        );
        if st.0 != 0 {
            let _ = LocalFree(Some(HLOCAL(psid.0)));
            return Err(SandboxError::Backend {
                label: "appcontainer".into(),
                backend: "appcontainer",
                detail: format!("GetNamedSecurityInfoW({}) failed: {st:?}", path.display()),
            });
        }

        let mut new_dacl: *mut ACL = ptr::null_mut();
        let st2 = SetEntriesInAclW(Some(&[ea]), Some(p_dacl as *const ACL), &mut new_dacl);
        if st2.0 != 0 {
            let _ = LocalFree(Some(HLOCAL(p_sd.0)));
            let _ = LocalFree(Some(HLOCAL(psid.0)));
            return Err(SandboxError::Backend {
                label: "appcontainer".into(),
                backend: "appcontainer",
                detail: format!("SetEntriesInAclW({}) failed: {st2:?}", path.display()),
            });
        }

        let st3 = SetNamedSecurityInfoW(
            PCWSTR(path_w.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(new_dacl as *const ACL),
            None,
        );
        let _ = LocalFree(Some(HLOCAL(new_dacl.cast())));
        let _ = LocalFree(Some(HLOCAL(p_sd.0)));
        let _ = LocalFree(Some(HLOCAL(psid.0)));
        if st3.0 != 0 {
            return Err(SandboxError::Backend {
                label: "appcontainer".into(),
                backend: "appcontainer",
                detail: format!(
                    "SetNamedSecurityInfoW(traverse {}) failed: {st3:?}",
                    path.display()
                ),
            });
        }
    }
    Ok(())
}

#[cfg(windows)]
fn quote_windows_arg(arg: &std::ffi::OsStr) -> String {
    let s = arg.to_string_lossy();
    if s.is_empty() {
        return "\"\"".to_string();
    }
    let needs_quotes = s.chars().any(|c| c == ' ' || c == '\t' || c == '"');
    if !needs_quotes {
        return s.into_owned();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0u32;
    for ch in s.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                for _ in 0..(backslashes * 2 + 1) {
                    out.push('\\');
                }
                out.push('"');
                backslashes = 0;
            }
            _ => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push(ch);
            }
        }
    }
    for _ in 0..(backslashes * 2) {
        out.push('\\');
    }
    out.push('"');
    out
}

/// Whether `path` sits under an OS tree where package ACE mutation is refused
/// (`SetNamedSecurityInfo` → ACCESS_DENIED for normal users).
#[cfg(windows)]
fn is_os_protected_path(path: &Path) -> bool {
    let candidate = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let lower = candidate
        .to_string_lossy()
        .to_ascii_lowercase()
        .replace('/', "\\");
    let trimmed = lower.strip_prefix(r"\\?\").unwrap_or(&lower);
    const ROOTS: &[&str] = &[
        r"c:\windows",
        r"c:\program files",
        r"c:\program files (x86)",
        r"c:\programdata\microsoft",
    ];
    ROOTS
        .iter()
        .any(|root| trimmed == *root || trimmed.starts_with(&format!("{root}\\")))
}

#[cfg(windows)]
fn grant_package_access(package_sid: &str, path: &Path, write: bool) -> Result<(), SandboxError> {
    use rappct::acl::{grant_to_package, AccessMask, ResourcePath};
    use rappct::AppContainerSid;

    if !write && is_os_protected_path(path) {
        tracing::debug!(
            path = %path.display(),
            "skipping AppContainer ACL grant on OS-protected path"
        );
        return Ok(());
    }

    let sid = AppContainerSid::from_sddl(package_sid);
    let access = if write {
        AccessMask(
            AccessMask::FILE_GENERIC_READ.0
                | AccessMask::FILE_GENERIC_WRITE.0
                | FILE_GENERIC_EXECUTE,
        )
    } else {
        AccessMask(AccessMask::FILE_GENERIC_READ.0 | FILE_GENERIC_EXECUTE)
    };
    let target = if path.is_dir() {
        ResourcePath::Directory(path.to_path_buf())
    } else {
        ResourcePath::File(path.to_path_buf())
    };
    grant_to_package(target, &sid, access).map_err(|err| SandboxError::Backend {
        label: "appcontainer".to_string(),
        backend: "appcontainer",
        detail: format!("ACL grant {}: {err}", path.display()),
    })
}

#[cfg(windows)]
fn revoke_package_access(path: &Path, package_sid: &str, is_dir: bool) -> Result<(), SandboxError> {
    use std::ptr;

    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertStringSidToSidW, GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW,
        EXPLICIT_ACCESS_W, REVOKE_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_SID,
        TRUSTEE_IS_WELL_KNOWN_GROUP, TRUSTEE_W,
    };
    use windows::Win32::Security::{ACE_FLAGS, ACL, DACL_SECURITY_INFORMATION, PSID};

    let sid_wide: Vec<u16> = package_sid
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut psid = PSID(ptr::null_mut());
    unsafe {
        ConvertStringSidToSidW(PCWSTR(sid_wide.as_ptr()), &mut psid).map_err(|err| {
            SandboxError::Backend {
                label: "appcontainer".to_string(),
                backend: "appcontainer",
                detail: format!("ConvertStringSidToSidW failed: {err}"),
            }
        })?;
    }
    let path_wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let mut p_sd = windows::Win32::Security::PSECURITY_DESCRIPTOR(ptr::null_mut());
        let mut p_dacl: *mut ACL = ptr::null_mut();
        let st = GetNamedSecurityInfoW(
            PCWSTR(path_wide.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut p_dacl),
            None,
            &mut p_sd,
        );
        if st.0 != 0 {
            let _ = LocalFree(Some(HLOCAL(psid.0)));
            return Err(SandboxError::Backend {
                label: "appcontainer".to_string(),
                backend: "appcontainer",
                detail: format!("GetNamedSecurityInfoW failed: {st:?}"),
            });
        }

        let mut trustee: TRUSTEE_W = std::mem::zeroed();
        trustee.TrusteeForm =
            windows::Win32::Security::Authorization::TRUSTEE_FORM(TRUSTEE_IS_SID.0);
        trustee.TrusteeType =
            windows::Win32::Security::Authorization::TRUSTEE_TYPE(TRUSTEE_IS_WELL_KNOWN_GROUP.0);
        trustee.ptstrName = PWSTR(psid.0 as *mut u16);

        let mut ea: EXPLICIT_ACCESS_W = std::mem::zeroed();
        ea.grfAccessPermissions = 0x001F_01FF; // GENERIC_ALL — revoke matching trustee
        ea.grfAccessMode = REVOKE_ACCESS;
        ea.grfInheritance = if is_dir {
            ACE_FLAGS(0x3) // SUB_CONTAINERS_AND_OBJECTS_INHERIT
        } else {
            ACE_FLAGS(0)
        };
        ea.Trustee = trustee;

        let mut new_dacl: *mut ACL = ptr::null_mut();
        let entries = [ea];
        let st2 = SetEntriesInAclW(Some(&entries), Some(p_dacl as *const ACL), &mut new_dacl);
        if st2.0 != 0 {
            let _ = LocalFree(Some(HLOCAL(p_sd.0)));
            let _ = LocalFree(Some(HLOCAL(psid.0)));
            return Err(SandboxError::Backend {
                label: "appcontainer".to_string(),
                backend: "appcontainer",
                detail: format!("SetEntriesInAclW(REVOKE) failed: {st2:?}"),
            });
        }

        let st3 = SetNamedSecurityInfoW(
            PCWSTR(path_wide.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(new_dacl as *const ACL),
            None,
        );
        let _ = LocalFree(Some(HLOCAL(new_dacl as *mut _)));
        let _ = LocalFree(Some(HLOCAL(p_sd.0)));
        let _ = LocalFree(Some(HLOCAL(psid.0)));
        if st3.0 != 0 {
            return Err(SandboxError::Backend {
                label: "appcontainer".to_string(),
                backend: "appcontainer",
                detail: format!("SetNamedSecurityInfoW(REVOKE) failed: {st3:?}"),
            });
        }
    }
    Ok(())
}

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Policy;

    #[test]
    fn deny_maps_to_no_network_caps() {
        let plan = plan_appcontainer(&Policy::new("t").net(NetPolicy::Deny)).unwrap();
        assert!(plan.capability_names.is_empty());
        assert_eq!(plan.profile_name, "bookclerk.t");
    }

    #[test]
    fn outbound_maps_to_internet_client() {
        let plan = plan_appcontainer(&Policy::new("t").net(NetPolicy::Outbound)).unwrap();
        assert_eq!(plan.capability_names, ["internetClient"]);
    }

    #[test]
    fn outbound_listen_adds_private_network() {
        let plan = plan_appcontainer(&Policy::new("t").net(NetPolicy::OutboundListen)).unwrap();
        assert_eq!(
            plan.capability_names,
            ["internetClient", "privateNetworkClientServer"]
        );
    }

    #[test]
    fn profile_name_sanitizes_plugin_labels() {
        assert_eq!(
            profile_name_for_label("plugin:libro"),
            "bookclerk.plugin.libro"
        );
        assert_eq!(profile_name_for_label("!!!"), "bookclerk.guest");
        assert!(profile_name_for_label(&"x".repeat(100)).len() <= 64);
    }
}
