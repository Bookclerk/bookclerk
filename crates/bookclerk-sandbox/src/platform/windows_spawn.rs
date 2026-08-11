//! Windows AppContainer spawn.
//!
//! Windows cannot confine a process after it has started. Isolation is granted
//! at `CreateProcess` by giving the child a token with an AppContainer SID and
//! named capability SIDs, then ACLing the **explicit** policy paths it may reach.
//!
//! # Security model
//!
//! - A normal AppContainer already receives limited read/execute access to
//!   selected OS resources through existing ACLs such as ALL APPLICATION
//!   PACKAGES. Loading `cmd.exe` or runtime DLLs from System32 is **not** the
//!   same as Bookclerk granting the guest access to System32.
//! - Bookclerk must **never** call `SetNamedSecurityInfo` / `grant_to_package`
//!   (or equivalents) on OS-managed trees (Windows, System32, WinSxS, Program
//!   Files, …). Writes under those roots are rejected before any ACL API.
//! - AppContainers run at low integrity; effective access is the intersection of
//!   the user’s rights and the Package SID’s rights (defense in depth).
//! - Every isolated launch gets a **unique** AppContainer profile/SID. Sharing a
//!   Package SID across concurrent jobs would merge ACL allowlists and make
//!   `REVOKE_ACCESS` tear down another job’s grants.
//! - Descendants inherit the AppContainer token and are placed in a kill-on-close
//!   Job Object so they cannot outlive profile cleanup.
//!
//! # `allow_exec` on Windows
//!
//! [`Policy::allow_exec`](crate::Policy::allow_exec) is **not** separately
//! enforceable at CreateProcess: AppContainer does not provide a Landlock-style
//! exec allowlist. Guests can still start other executables that the OS and
//! ACL allowlist make reachable. Treat `allow_exec = false` as advisory on
//! Windows; path ACLs and low integrity remain the boundary.
//!
//! # Child process creation
//!
//! There is no extra Win32 switch that forbids `CreateProcess` inside the
//! container. Descendants stay in the same AppContainer and Job Object; when
//! the primary guest exits (or the job handle closes), the tree is terminated
//! before the profile is deleted.

#![cfg_attr(windows, allow(unsafe_code))] // Win32 ACL revoke uses raw SID/ACL APIs.

use std::ffi::OsString;
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;

use crate::{NetPolicy, Policy, SandboxError};

/// Pure AppContainer launch plan (capabilities only — no profile side effects).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppContainerLaunch {
    /// Sanitized diagnostics stem derived from the policy label (not a moniker).
    pub label_stem: String,
    /// Named capability SIDs to grant (well-known capability names).
    pub capability_names: Vec<&'static str>,
}

/// Map a confinement [`Policy`] into AppContainer network capabilities.
///
/// This is pure: it does **not** create or ensure an AppContainer profile.
/// Create a profile with [`AppContainerSession::create`] (or attach a host-owned
/// one) before [`run_appcontainer`].
///
/// | [`NetPolicy`] | Capabilities |
/// | --- | --- |
/// | [`Deny`](NetPolicy::Deny) | _(none)_ |
/// | [`Outbound`](NetPolicy::Outbound) | `internetClient` |
/// | [`OutboundListen`](NetPolicy::OutboundListen) | `internetClient`, `privateNetworkClientServer` |
/// | [`Full`](NetPolicy::Full) | `internetClient`, `internetClientServer`, `privateNetworkClientServer` |
#[must_use]
pub fn plan_appcontainer(policy: &Policy) -> AppContainerLaunch {
    AppContainerLaunch {
        label_stem: label_stem_for_diagnostics(policy.label()),
        capability_names: capability_names_for(policy.net_policy()),
    }
}

/// Sanitize a policy label into a diagnostics-friendly stem (not a full moniker).
///
/// Prefer [`unique_profile_moniker`] for CreateAppContainerProfile names.
#[must_use]
pub fn profile_name_for_label(label: &str) -> String {
    label_stem_for_diagnostics(label)
}

fn label_stem_for_diagnostics(label: &str) -> String {
    let mut out = String::new();
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
    if trimmed.is_empty() {
        "guest".to_string()
    } else {
        trimmed
    }
}

/// Build a collision-resistant AppContainer profile moniker (≤ 64 characters).
///
/// Format: `bc.<sanitized-label-stem>.<16 hex random>`. Concurrent launches with
/// the same policy label always receive distinct Package SIDs.
#[must_use]
pub fn unique_profile_moniker(label: &str) -> String {
    use rand::RngCore;

    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    let suffix: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    // "bc." + stem + "." + 16 hex ≤ 64
    const PREFIX: &str = "bc.";
    let budget = 64usize
        .saturating_sub(PREFIX.len())
        .saturating_sub(1)
        .saturating_sub(suffix.len());
    let mut stem = label_stem_for_diagnostics(label);
    if stem.len() > budget {
        stem.truncate(budget);
        stem = stem.trim_end_matches('.').to_string();
    }
    if stem.is_empty() {
        stem = "guest".to_string();
        if stem.len() > budget {
            stem.truncate(budget);
        }
    }
    format!("{PREFIX}{stem}.{suffix}")
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

/// Owned AppContainer profile for one isolated launch.
///
/// When [`delete_on_drop`](Self::delete_on_drop) is true (the default for
/// [`Self::create`]), dropping the session deletes the profile after the guest
/// and its Job Object have exited. Host-owned sessions created for long-lived
/// plugins use [`Self::attach`] so the jail does not delete the profile.
#[derive(Debug)]
pub struct AppContainerSession {
    profile_name: String,
    package_sid: String,
    delete_on_drop: bool,
}

impl AppContainerSession {
    /// Create a fresh profile with a unique moniker derived from `label`.
    pub fn create(label: &str) -> Result<Self, SandboxError> {
        let profile_name = unique_profile_moniker(label);
        Self::ensure_named(&profile_name, label, true)
    }

    /// Open (ensure) an existing profile without taking deletion ownership.
    pub fn attach(profile_name: &str) -> Result<Self, SandboxError> {
        Self::ensure_named(profile_name, profile_name, false)
    }

    /// Profile moniker passed to CreateAppContainerProfile.
    #[must_use]
    pub fn profile_name(&self) -> &str {
        &self.profile_name
    }

    /// Package SID as an SDDL string.
    #[must_use]
    pub fn package_sid(&self) -> &str {
        &self.package_sid
    }

    /// Whether [`Drop`] will call DeleteAppContainerProfile.
    #[must_use]
    pub fn delete_on_drop(&self) -> bool {
        self.delete_on_drop
    }

    /// Relinquish deletion ownership (e.g. transfer to another owner).
    pub fn disarm_delete(&mut self) {
        self.delete_on_drop = false;
    }

    /// Take deletion ownership (DeleteAppContainerProfile on drop).
    pub fn arm_delete(&mut self) {
        self.delete_on_drop = true;
    }

    fn ensure_named(
        profile_name: &str,
        display_label: &str,
        delete_on_drop: bool,
    ) -> Result<Self, SandboxError> {
        #[cfg(windows)]
        {
            use rappct::AppContainerProfile;

            if profile_name.len() > 64 {
                return Err(SandboxError::Backend {
                    label: display_label.to_string(),
                    backend: "appcontainer",
                    detail: format!(
                        "AppContainer profile name exceeds 64 characters ({})",
                        profile_name.len()
                    ),
                });
            }
            let profile = AppContainerProfile::ensure(
                profile_name,
                &format!("Bookclerk {display_label}"),
                Some("Bookclerk plugin / media guest AppContainer"),
            )
            .map_err(|err| SandboxError::Backend {
                label: display_label.to_string(),
                backend: "appcontainer",
                detail: format!("CreateAppContainerProfile failed: {err}"),
            })?;
            Ok(Self {
                profile_name: profile_name.to_string(),
                package_sid: profile.sid.as_string().to_string(),
                delete_on_drop,
            })
        }
        #[cfg(not(windows))]
        {
            let _ = (profile_name, display_label, delete_on_drop);
            Err(SandboxError::Backend {
                label: display_label.to_string(),
                backend: "appcontainer",
                detail: "AppContainer profiles require Windows".to_string(),
            })
        }
    }

    #[cfg(windows)]
    fn delete_profile(&self) {
        use rappct::AppContainerProfile;
        use rappct::AppContainerSid;

        let profile = AppContainerProfile {
            name: self.profile_name.clone(),
            sid: AppContainerSid::from_sddl(&self.package_sid),
        };
        if let Err(err) = profile.delete() {
            tracing::warn!(
                profile = %self.profile_name,
                error = %err,
                "failed to delete AppContainer profile"
            );
        }
    }
}

impl Drop for AppContainerSession {
    fn drop(&mut self) {
        if self.delete_on_drop {
            #[cfg(windows)]
            self.delete_profile();
        }
    }
}

/// RAII guard that revokes a temporary Package-SID ACE on drop.
///
/// With a per-launch Package SID, [`REVOKE_ACCESS`] removes only this launch’s
/// trustee ACEs on the path (not another job’s grants).
#[derive(Debug)]
pub struct AclGrant {
    #[cfg(windows)]
    path: PathBuf,
    #[cfg(windows)]
    package_sid: String,
    #[cfg(windows)]
    is_dir: bool,
    /// False for ambient OS runtime paths where no ACE was written.
    #[cfg(windows)]
    active: bool,
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

    /// Whether an ACE was actually written (false for ambient OS skips).
    #[must_use]
    pub fn is_active(&self) -> bool {
        #[cfg(windows)]
        {
            self.active
        }
        #[cfg(not(windows))]
        {
            false
        }
    }
}

impl Drop for AclGrant {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            if !self.active {
                return;
            }
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
///
/// Ambient OS runtime paths: read grants are no-ops (OS ALL APPLICATION PACKAGES
/// already covers them); write grants fail closed without calling an ACL API.
pub fn grant_path_access(
    package_sid: &str,
    path: &Path,
    write: bool,
) -> Result<AclGrant, SandboxError> {
    #[cfg(windows)]
    {
        match classify_acl_path(path) {
            AclPathClass::AmbientOsRuntime if write => {
                return Err(SandboxError::Backend {
                    label: "appcontainer".to_string(),
                    backend: "appcontainer",
                    detail: format!(
                        "refusing ACL write grant under OS-managed path {}",
                        path.display()
                    ),
                });
            }
            AclPathClass::AmbientOsRuntime => {
                tracing::debug!(
                    path = %path.display(),
                    "skipping AppContainer ACL grant on ambient OS runtime path"
                );
                return Ok(AclGrant {
                    path: path.to_path_buf(),
                    package_sid: package_sid.to_string(),
                    is_dir: path.is_dir(),
                    active: false,
                });
            }
            AclPathClass::Explicit => {}
        }
        grant_package_access(package_sid, path, write)?;
        Ok(AclGrant {
            path: path.to_path_buf(),
            package_sid: package_sid.to_string(),
            is_dir: path.is_dir(),
            active: true,
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
/// When `session` is `None`, a unique profile is created for this launch and
/// deleted after the process tree exits. When `Some`, the caller owns the
/// profile (typical for long-lived plugin guests).
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
    session: Option<&AppContainerSession>,
) -> Result<u32, SandboxError> {
    #[cfg(windows)]
    {
        run_appcontainer_windows(policy, program, args, session)
    }
    #[cfg(not(windows))]
    {
        let _ = (program, args, session);
        let _ = plan_appcontainer(policy);
        Err(SandboxError::Backend {
            label: policy.label().to_string(),
            backend: "appcontainer",
            detail: "AppContainer CreateProcess is only available on Windows".to_string(),
        })
    }
}

/// Whether `path` sits under an OS-managed tree where Bookclerk must never
/// mutate DACLs.
#[cfg(windows)]
#[must_use]
pub fn is_os_managed_path(path: &Path) -> bool {
    matches!(classify_acl_path(path), AclPathClass::AmbientOsRuntime)
}

#[cfg(not(windows))]
#[must_use]
pub fn is_os_managed_path(_path: &Path) -> bool {
    false
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AclPathClass {
    /// Windows / System32 / WinSxS / Program Files / … — never mutate ACLs.
    AmbientOsRuntime,
    /// Explicit policy/user path — ACL grants are Bookclerk’s responsibility.
    Explicit,
}

#[cfg(windows)]
fn classify_acl_path(path: &Path) -> AclPathClass {
    let candidate = normalize_path(path);
    let roots = match os_managed_roots_or_err() {
        Ok(r) => r,
        // Fail closed: treat every path as OS-managed so ACL APIs are not called.
        Err(_) => return AclPathClass::AmbientOsRuntime,
    };
    for root in roots {
        if path_is_within(&candidate, &root) {
            return AclPathClass::AmbientOsRuntime;
        }
    }
    AclPathClass::Explicit
}

/// Known OS roots from system / Known Folder APIs (env vars are not authoritative).
///
/// Failure to resolve critical roots is fail-closed for ACL mutation.
#[cfg(windows)]
fn os_managed_roots_or_err() -> Result<Vec<PathBuf>, SandboxError> {
    use windows::Win32::UI::Shell::{
        FOLDERID_ProgramData, FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX86,
        SHGetKnownFolderPath, KF_FLAG_DEFAULT,
    };

    let mut roots = Vec::new();
    let win = windows_directory().ok_or_else(|| SandboxError::Backend {
        label: "appcontainer".into(),
        backend: "appcontainer",
        detail: "GetWindowsDirectoryW failed; refusing ACL mutation without OS roots".into(),
    })?;
    push_unique_root(&mut roots, &win);
    push_unique_root(&mut roots, &win.join("System32"));
    push_unique_root(&mut roots, &win.join("SysWOW64"));
    push_unique_root(&mut roots, &win.join("WinSxS"));
    if let Some(sys) = system_directory() {
        push_unique_root(&mut roots, &sys);
    }
    unsafe {
        for folder in [
            &FOLDERID_ProgramFiles,
            &FOLDERID_ProgramFilesX86,
            &FOLDERID_ProgramData,
        ] {
            if let Ok(pwstr) = SHGetKnownFolderPath(folder, KF_FLAG_DEFAULT, None) {
                if !pwstr.is_null() {
                    if let Ok(s) = pwstr.to_string() {
                        let path = PathBuf::from(s);
                        if *folder == FOLDERID_ProgramData {
                            push_unique_root(&mut roots, &path.join("Microsoft"));
                        } else {
                            push_unique_root(&mut roots, &path);
                        }
                    }
                    windows::Win32::System::Com::CoTaskMemFree(Some(pwstr.0.cast()));
                }
            }
        }
    }

    if roots.is_empty() {
        return Err(SandboxError::Backend {
            label: "appcontainer".into(),
            backend: "appcontainer",
            detail: "no OS-managed roots resolved; refusing ACL mutation".into(),
        });
    }
    Ok(roots)
}

#[cfg(windows)]
fn windows_directory() -> Option<PathBuf> {
    use windows::Win32::System::SystemInformation::GetWindowsDirectoryW;
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowsDirectoryW(Some(&mut buf)) };
    if n == 0 || (n as usize) >= buf.len() {
        return None;
    }
    Some(PathBuf::from(String::from_utf16_lossy(&buf[..n as usize])))
}

#[cfg(windows)]
fn system_directory() -> Option<PathBuf> {
    use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut buf = [0u16; 512];
    let n = unsafe { GetSystemDirectoryW(Some(&mut buf)) };
    if n == 0 || (n as usize) >= buf.len() {
        return None;
    }
    Some(PathBuf::from(String::from_utf16_lossy(&buf[..n as usize])))
}

#[cfg(windows)]
fn push_unique_root(roots: &mut Vec<PathBuf>, path: &Path) {
    let normalized = normalize_path(path);
    if normalized.as_os_str().is_empty() {
        return;
    }
    if !roots.iter().any(|existing| existing == &normalized) {
        roots.push(normalized);
    }
}

#[cfg(windows)]
fn normalize_path(path: &Path) -> PathBuf {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    strip_verbatim_prefix(canonical)
}

#[cfg(windows)]
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(stripped) = text.strip_prefix(r"\\?\") {
        PathBuf::from(stripped)
    } else {
        path
    }
}

/// Component-wise containment (case-insensitive on Windows).
#[cfg(windows)]
fn path_is_within(path: &Path, root: &Path) -> bool {
    let path_comps: Vec<String> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect();
    let root_comps: Vec<String> = root
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect();
    if root_comps.is_empty() || path_comps.len() < root_comps.len() {
        return false;
    }
    path_comps
        .iter()
        .zip(root_comps.iter())
        .all(|(a, b)| a == b)
}

#[cfg(windows)]
fn run_appcontainer_windows(
    policy: &Policy,
    program: &Path,
    args: &[OsString],
    session: Option<&AppContainerSession>,
) -> Result<u32, SandboxError> {
    use std::io::{self, Read, Write};
    use std::thread;
    use std::time::Duration;

    use rappct::acl::{grant_to_package, AccessMask, ResourcePath};
    use rappct::{AppContainerProfile, AppContainerSid, SecurityCapabilitiesBuilder};

    use super::windows_launch::{launch_appcontainer_guest, LaunchRequest};

    let plan = plan_appcontainer(policy);
    let owned_session;
    let session = match session {
        Some(existing) => existing,
        None => {
            owned_session = AppContainerSession::create(policy.label())?;
            &owned_session
        }
    };
    let package_sid = session.package_sid().to_string();
    let profile_name = session.profile_name().to_string();

    let profile = AppContainerProfile {
        name: profile_name.clone(),
        sid: AppContainerSid::from_sddl(&package_sid),
    };

    let mut builder = SecurityCapabilitiesBuilder::new(&profile.sid);
    if !plan.capability_names.is_empty() {
        builder = builder.with_named(&plan.capability_names);
    }
    let sec = builder.build().map_err(|err| SandboxError::Backend {
        label: policy.label().to_string(),
        backend: "appcontainer",
        detail: format!("capability SID derivation failed: {err}"),
    })?;

    // Fail closed on OS-managed write grants before mutating any DACL.
    for path in policy.resolved_writes() {
        if matches!(classify_acl_path(&path), AclPathClass::AmbientOsRuntime) {
            return Err(SandboxError::Backend {
                label: policy.label().to_string(),
                backend: "appcontainer",
                detail: format!(
                    "refusing ACL write grant under OS-managed path {}",
                    path.display()
                ),
            });
        }
    }

    // Keep grants for the life of the launch; revoke afterward (unique SID ⇒
    // REVOKE_ACCESS cannot tear down another launch’s trustee).
    let mut grants: Vec<AclGrant> = Vec::new();
    let mut allowlisted: Vec<PathBuf> = Vec::new();
    let mut granted_paths = std::collections::HashSet::new();

    // Writes first so a path in both lists is not double-granted.
    for path in policy.resolved_writes() {
        if !granted_paths.insert(path.clone()) {
            continue;
        }
        let access = AccessMask(
            AccessMask::FILE_GENERIC_READ.0
                | AccessMask::FILE_GENERIC_WRITE.0
                | FILE_GENERIC_EXECUTE,
        );
        let is_dir = path.is_dir();
        let target = if is_dir {
            ResourcePath::Directory(path.clone())
        } else {
            ResourcePath::File(path.clone())
        };
        {
            let _lock = acl_api_lock();
            grant_to_package(target, &profile.sid, access).map_err(|err| {
                SandboxError::Backend {
                    label: policy.label().to_string(),
                    backend: "appcontainer",
                    detail: format!("ACL grant (write) {}: {err}", path.display()),
                }
            })?;
        }
        allowlisted.push(path.clone());
        grants.push(AclGrant {
            path,
            package_sid: package_sid.clone(),
            is_dir,
            active: true,
        });
    }

    for path in policy.resolved_reads() {
        if !granted_paths.insert(path.clone()) {
            continue;
        }
        match classify_acl_path(&path) {
            AclPathClass::AmbientOsRuntime => {
                tracing::debug!(
                    path = %path.display(),
                    "skipping AppContainer ACL grant on ambient OS runtime path"
                );
                continue;
            }
            AclPathClass::Explicit => {}
        }
        let access = AccessMask(AccessMask::FILE_GENERIC_READ.0 | FILE_GENERIC_EXECUTE);
        let is_dir = path.is_dir();
        let target = if is_dir {
            ResourcePath::Directory(path.clone())
        } else {
            ResourcePath::File(path.clone())
        };
        {
            let _lock = acl_api_lock();
            grant_to_package(target, &profile.sid, access).map_err(|err| {
                SandboxError::Backend {
                    label: policy.label().to_string(),
                    backend: "appcontainer",
                    detail: format!("ACL grant (read) {}: {err}", path.display()),
                }
            })?;
        }
        allowlisted.push(path.clone());
        grants.push(AclGrant {
            path,
            package_sid: package_sid.clone(),
            is_dir,
            active: true,
        });
    }

    // AppContainers cannot walk into a granted leaf without FILE_TRAVERSE on
    // each ancestor. Directory grants inherit onto children, so ancestors get
    // no-inheritance traverse only — never `%TEMP%` itself.
    let mut seen_ancestors = std::collections::HashSet::new();
    for path in &allowlisted {
        for ancestor in ancestor_directories(path) {
            if !seen_ancestors.insert(ancestor.clone()) {
                continue;
            }
            if matches!(classify_acl_path(&ancestor), AclPathClass::AmbientOsRuntime) {
                continue;
            }
            match grant_directory_traverse_no_inherit(&package_sid, &ancestor) {
                Ok(()) => grants.push(AclGrant {
                    path: ancestor,
                    package_sid: package_sid.clone(),
                    is_dir: true,
                    active: true,
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

    // Grant the executable when it is an explicit (non-OS) path. Never ACE
    // System32 binaries — ALL APPLICATION PACKAGES covers those.
    if program.exists()
        && matches!(classify_acl_path(program), AclPathClass::Explicit)
        && granted_paths.insert(program.to_path_buf())
    {
        let access = AccessMask(AccessMask::FILE_GENERIC_READ.0 | FILE_GENERIC_EXECUTE);
        let target = ResourcePath::File(program.to_path_buf());
        let grant_result = {
            let _lock = acl_api_lock();
            grant_to_package(target, &profile.sid, access)
        };
        match grant_result {
            Ok(()) => grants.push(AclGrant {
                path: program.to_path_buf(),
                package_sid: package_sid.clone(),
                is_dir: false,
                active: true,
            }),
            Err(err) => {
                tracing::debug!(
                    path = %program.display(),
                    error = %err,
                    "optional ACL grant for guest executable failed"
                );
            }
        }
    }

    let (cwd, child_env) = appcontainer_child_context(&profile, policy)?;

    // Host-created profile / Temp dirs may lack Package SID ACEs (CreateAppContainerProfile
    // ACLs the root; create_dir_all as the host user does not). Grant write for the
    // launch lifetime so guest TEMP round-trips succeed.
    for path in [&cwd, &cwd.join("Temp")] {
        if path.exists() && granted_paths.insert(path.clone()) {
            match grant_package_access(&package_sid, path, true) {
                Ok(()) => grants.push(AclGrant {
                    path: path.clone(),
                    package_sid: package_sid.clone(),
                    is_dir: true,
                    active: true,
                }),
                Err(err) => {
                    drop(grants);
                    return Err(SandboxError::Backend {
                        label: policy.label().to_string(),
                        backend: "appcontainer",
                        detail: format!(
                            "ACL grant for AppContainer profile path {}: {err}",
                            path.display()
                        ),
                    });
                }
            }
        }
    }

    // CreateProcess: lpApplicationName = exe, lpCommandLine must still begin with
    // argv[0] (the program image) so Rust/C argv parsing lines up.
    let cmdline = windows_command_line(program, args);
    let job_limits = job_limits_for_policy(policy);
    let request = LaunchRequest {
        exe: program,
        cmdline,
        cwd,
        env: child_env,
        sec: &sec,
        job: job_limits,
    };

    let mut io = match launch_appcontainer_guest(request) {
        Ok(io) => io,
        Err(err) => {
            drop(grants);
            return Err(SandboxError::Backend {
                label: policy.label().to_string(),
                backend: "appcontainer",
                detail: format!("CreateProcess AppContainer failed: {err}"),
            });
        }
    };

    let mut child_stdin = io.stdin.take();
    let mut child_stdout = io.stdout.take();
    let mut child_stderr = io.stderr.take();

    // Stdin copier: after the guest exits we must not block forever on the
    // jail's still-open stdin (plugin hosts keep the write end alive). Detach
    // once wait returns so ACL revoke / profile delete can proceed.
    let t_in = thread::spawn(move || {
        if let Some(mut dest) = child_stdin.take() {
            let _ = io::copy(&mut io::stdin(), &mut dest);
            let _ = dest.flush();
        }
    });
    // Flush after every chunk: when the jail's stdout is a pipe (CI, plugin
    // host), the runtime may fully buffer; JSON-RPC lines must reach the parent
    // before the guest exits.
    let t_out = thread::spawn(move || {
        if let Some(mut src) = child_stdout.take() {
            let _ = copy_flushing(&mut src, &mut io::stdout());
        }
    });
    let stderr_budget = stderr_byte_budget_for_label(policy.label());
    let t_err = thread::spawn(move || {
        if let Some(src) = child_stderr.take() {
            let mut limited = src.take(stderr_budget);
            let _ = copy_flushing(&mut limited, &mut io::stderr());
        }
    });

    // Dropping `io` after wait closes the Job (kill-on-close) before ACL/profile
    // cleanup so descendants cannot outlive grants or DeleteAppContainerProfile.
    let wait_result = io.wait(None);
    // Drain stdout/stderr briefly, then detach. Joining forever deadlocks when
    // the parent stopped reading (full pipe) — the same lifecycle that keeps
    // stdin open after the guest exits. ACL revoke must not wait on that.
    join_proxy_timeout(t_out, Duration::from_secs(2));
    join_proxy_timeout(t_err, Duration::from_secs(2));
    drop(t_in);
    let code = match wait_result {
        Ok(code) => code,
        Err(err) => {
            drop(grants);
            return Err(SandboxError::Backend {
                label: policy.label().to_string(),
                backend: "appcontainer",
                detail: format!("waiting for AppContainer guest failed: {err}"),
            });
        }
    };
    drop(grants);

    tracing::debug!(
        label = %policy.label(),
        profile = %profile_name,
        package_sid = %package_sid,
        exit = code,
        "AppContainer guest exited"
    );
    Ok(code)
}

/// Join a stdio proxy briefly after the guest exits; detach if it does not
/// finish (parent pipe full / unread).
#[cfg(windows)]
fn join_proxy_timeout(handle: std::thread::JoinHandle<()>, limit: std::time::Duration) {
    let start = std::time::Instant::now();
    loop {
        if handle.is_finished() {
            let _ = handle.join();
            return;
        }
        if start.elapsed() >= limit {
            drop(handle);
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Copy bytes and flush the writer after each successful read so parents that
/// poll stdout mid-flight (JSON-RPC) are not stuck behind a full
/// stdio buffer until guest EOF.
#[cfg(windows)]
fn copy_flushing<R: std::io::Read + ?Sized, W: std::io::Write + ?Sized>(
    reader: &mut R,
    writer: &mut W,
) -> std::io::Result<u64> {
    let mut buf = [0u8; 8 * 1024];
    let mut written = 0u64;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                writer.flush()?;
                return Ok(written);
            }
            Ok(n) => {
                writer.write_all(&buf[..n])?;
                writer.flush()?;
                written += n as u64;
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
}

/// Job resource limits: Spec fields override label heuristics.
#[cfg(windows)]
fn job_limits_for_policy(policy: &Policy) -> super::windows_launch::JobResourceLimits {
    use super::windows_launch::JobResourceLimits;
    let limits = policy.resolved_job_limits();
    JobResourceLimits {
        memory_bytes: limits
            .memory_bytes
            .map(|b| usize::try_from(b).unwrap_or(usize::MAX)),
        cpu_rate_percent: limits.cpu_rate_percent,
        active_processes: limits.active_processes,
    }
}

/// Cap proxied stderr so a noisy guest cannot pin the jail host indefinitely.
#[cfg(windows)]
fn stderr_byte_budget_for_label(label: &str) -> u64 {
    if label.to_ascii_lowercase().contains("media") {
        16 * 1024 * 1024
    } else {
        1024 * 1024
    }
}

/// Resolve AppContainer-local cwd / TEMP and a deliberate child environment.
#[cfg(windows)]
fn appcontainer_child_context(
    profile: &rappct::AppContainerProfile,
    policy: &Policy,
) -> Result<(PathBuf, Vec<(OsString, OsString)>), SandboxError> {
    let folder = resolve_appcontainer_folder(profile, policy)?;
    std::fs::create_dir_all(&folder).map_err(|err| SandboxError::Backend {
        label: policy.label().to_string(),
        backend: "appcontainer",
        detail: format!(
            "could not create AppContainer folder {}: {err}",
            folder.display()
        ),
    })?;
    let temp = folder.join("Temp");
    std::fs::create_dir_all(&temp).map_err(|err| SandboxError::Backend {
        label: policy.label().to_string(),
        backend: "appcontainer",
        detail: format!(
            "could not create AppContainer Temp {}: {err}",
            temp.display()
        ),
    })?;

    let mut env: Vec<(OsString, OsString)> = vec![
        (
            OsString::from("LOCALAPPDATA"),
            folder.as_os_str().to_os_string(),
        ),
        (OsString::from("TEMP"), temp.as_os_str().to_os_string()),
        (OsString::from("TMP"), temp.as_os_str().to_os_string()),
    ];
    // Required Windows runtime variables only — never reintroduce Bookclerk secrets.
    for key in [
        "SystemRoot",
        "windir",
        "SystemDrive",
        "ComSpec",
        "PATH",
        "PATHEXT",
        "NUMBER_OF_PROCESSORS",
        "PROCESSOR_ARCHITECTURE",
    ] {
        if let Some(value) = std::env::var_os(key) {
            env.push((OsString::from(key), value));
        }
    }
    Ok((folder, env))
}

/// Resolve the AppContainer profile folder via `GetAppContainerFolderPath`.
///
/// Docs describe `%LOCALAPPDATA%\Packages\<moniker>\AC`. On current Windows CI,
/// the API returns `%LOCALAPPDATA%\Packages\<package-SID>` (no `\AC` suffix).
/// The API result is authoritative when it sits under Known Folder
/// LocalAppData\Packages; Bookclerk never synthesizes a Packages path on failure.
#[cfg(windows)]
fn resolve_appcontainer_folder(
    profile: &rappct::AppContainerProfile,
    policy: &Policy,
) -> Result<PathBuf, SandboxError> {
    let packages_root = host_local_app_data().map(|base| base.join("Packages"));
    let sid = profile.sid.as_string();
    match profile.folder_path() {
        Ok(path)
            if path_is_safe_appcontainer_folder(
                &path,
                &profile.name,
                sid,
                packages_root.as_deref(),
            ) =>
        {
            // Measured: GetAppContainerFolderPath → Packages\<SID>.
            // Documented / OS env redirect → Packages\<moniker>\AC.
            // Create both (API root + moniker\AC) so host prep and guest TEMP agree.
            let api_ac = path.join("AC");
            std::fs::create_dir_all(api_ac.join("Temp")).map_err(|err| SandboxError::Backend {
                label: policy.label().to_string(),
                backend: "appcontainer",
                detail: format!(
                    "could not create AppContainer API Temp {}: {err}",
                    api_ac.join("Temp").display()
                ),
            })?;
            if let Some(root) = packages_root.as_deref() {
                let moniker_ac = root.join(&profile.name).join("AC");
                std::fs::create_dir_all(moniker_ac.join("Temp")).map_err(|err| {
                    SandboxError::Backend {
                        label: policy.label().to_string(),
                        backend: "appcontainer",
                        detail: format!(
                            "could not create AppContainer moniker Temp {}: {err}",
                            moniker_ac.join("Temp").display()
                        ),
                    }
                })?;
                // Prefer moniker\AC for cwd/LOCALAPPDATA — matches OS TEMP redirect.
                Ok(moniker_ac)
            } else {
                Ok(api_ac)
            }
        }
        Ok(path) => Err(SandboxError::Backend {
            label: policy.label().to_string(),
            backend: "appcontainer",
            detail: format!(
                "GetAppContainerFolderPath returned unsafe path {} \
                 (expected under LocalAppData\\Packages\\ for moniker {} / SID {})",
                path.display(),
                profile.name,
                sid
            ),
        }),
        Err(err) => Err(SandboxError::Backend {
            label: policy.label().to_string(),
            backend: "appcontainer",
            detail: format!(
                "GetAppContainerFolderPath failed ({err}); refusing to synthesize a profile path"
            ),
        }),
    }
}

/// Host LocalAppData via Known Folder API (not mutable environment variables).
#[cfg(windows)]
fn host_local_app_data() -> Option<PathBuf> {
    use windows::core::PWSTR;
    use windows::Win32::UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath, KF_FLAG_DEFAULT};

    unsafe {
        let pwstr: PWSTR =
            SHGetKnownFolderPath(&FOLDERID_LocalAppData, KF_FLAG_DEFAULT, None).ok()?;
        if pwstr.is_null() {
            return None;
        }
        let path = pwstr.to_string().ok().map(PathBuf::from);
        windows::Win32::System::Com::CoTaskMemFree(Some(pwstr.0.cast()));
        path
    }
}

/// Validate GetAppContainerFolderPath under Known Folder Packages.
///
/// Accepts either the documented `Packages\<moniker>\AC` layout or the measured
/// `Packages\<package-SID>` / `Packages\<package-SID>\AC` return from the API.
#[cfg(any(test, windows))]
fn path_is_safe_appcontainer_folder(
    path: &Path,
    moniker: &str,
    package_sid: &str,
    packages_root: Option<&Path>,
) -> bool {
    let lower = path
        .to_string_lossy()
        .to_ascii_lowercase()
        .replace('/', "\\");
    let moniker_l = moniker.to_ascii_lowercase();
    let sid_l = package_sid.to_ascii_lowercase();
    if moniker_l.is_empty() && sid_l.is_empty() {
        return false;
    }
    // Reject nested Packages remapping / junction smuggling.
    if lower.matches("\\packages\\").count() != 1 {
        return false;
    }
    let moniker_ok = !moniker_l.is_empty()
        && (lower.contains(&format!("\\packages\\{moniker_l}\\ac"))
            || lower.ends_with(&format!("\\packages\\{moniker_l}"))
            || lower.ends_with(&format!("\\packages\\{moniker_l}\\ac")));
    let sid_ok = !sid_l.is_empty()
        && (lower.contains(&format!("\\packages\\{sid_l}"))
            && !lower.contains(&format!("\\packages\\{sid_l}\\packages\\")));
    if !moniker_ok && !sid_ok {
        return false;
    }
    if let Some(root) = packages_root {
        let root_l = root
            .to_string_lossy()
            .to_ascii_lowercase()
            .replace('/', "\\");
        if !lower.starts_with(&root_l) {
            return false;
        }
    }
    true
}

/// `FILE_GENERIC_EXECUTE` (not always re-exported by every windows-rs feature set).
#[cfg(windows)]
const FILE_GENERIC_EXECUTE: u32 = 0x0012_00A0;

/// Cross-process + in-process serialization for Win32 DACL mutations.
///
/// Concurrent `SetNamedSecurityInfo` / `SetEntriesInAcl` on the same path (e.g.
/// a shared media output directory used by overlapping jobs) races and surfaces
/// as `ERROR_ACCESS_DENIED`. Unique Package SIDs do not remove that race.
/// A process-local `Mutex` alone is insufficient: each `bookclerk-jail` child
/// (and separate CLI/daemon instances) can race on the same directory. The
/// named mutex `Local\bookclerk-dacl-tx` (session-local namespace) covers every
/// complete DACL read/modify/write; acquisition uses a 30s timeout and fails
/// closed with an actionable error.
#[cfg(windows)]
fn acl_api_lock() -> AclApiLock {
    match AclApiLock::acquire() {
        Ok(guard) => guard,
        Err(err) => panic!("ACL named mutex required for DACL mutation: {err}"),
    }
}

#[cfg(windows)]
pub struct AclApiLock {
    _local: std::sync::MutexGuard<'static, ()>,
    named: windows::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl AclApiLock {
    fn acquire() -> Result<Self, SandboxError> {
        use std::sync::Mutex;

        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{
            CloseHandle, WAIT_ABANDONED_0, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
        };
        use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};

        static LOCK: Mutex<()> = Mutex::new(());
        let local = LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let name: Vec<u16> = "Local\\bookclerk-dacl-tx"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // `Local\` is per-session. Default DACL from the creating user denies
        // other users; we do not grant World/Everyone access.
        let mutex = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }.map_err(|err| {
            SandboxError::Backend {
                label: "appcontainer".into(),
                backend: "appcontainer",
                detail: format!("CreateMutexW(Local\\bookclerk-dacl-tx) failed: {err}"),
            }
        })?;

        const ACL_MUTEX_TIMEOUT_MS: u32 = 30_000;
        let wait = unsafe { WaitForSingleObject(mutex, ACL_MUTEX_TIMEOUT_MS) };
        if wait == WAIT_FAILED {
            let _ = unsafe { CloseHandle(mutex) };
            return Err(SandboxError::Backend {
                label: "appcontainer".into(),
                backend: "appcontainer",
                detail: "WaitForSingleObject(Local\\bookclerk-dacl-tx) failed".into(),
            });
        }
        if wait == WAIT_TIMEOUT {
            let _ = unsafe { CloseHandle(mutex) };
            return Err(SandboxError::Backend {
                label: "appcontainer".into(),
                backend: "appcontainer",
                detail: "timed out after 30s waiting for Local\\bookclerk-dacl-tx \
                         (another Bookclerk process is mutating DACLs)"
                    .into(),
            });
        }
        if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED_0 {
            let _ = unsafe { CloseHandle(mutex) };
            return Err(SandboxError::Backend {
                label: "appcontainer".into(),
                backend: "appcontainer",
                detail: format!("unexpected wait result for ACL mutex: {wait:?}"),
            });
        }

        Ok(Self {
            _local: local,
            named: mutex,
        })
    }
}

#[cfg(windows)]
impl Drop for AclApiLock {
    fn drop(&mut self) {
        unsafe {
            use windows::Win32::Foundation::{CloseHandle, HANDLE};
            use windows::Win32::System::Threading::ReleaseMutex;
            let _ = ReleaseMutex(self.named);
            let _ = CloseHandle(self.named);
            self.named = HANDLE::default();
        }
    }
}

/// Build CreateProcess `lpCommandLine` including argv[0].
///
/// The argument immediately after `cmd`'s `/C` or `/K` is joined **raw**: wrapping
/// a multi-word script in quotes triggers cmd's quote rule and breaks `&&`
/// chains. Embedded paths inside that script must already be quoted by the caller.
#[cfg(windows)]
fn windows_command_line(program: &Path, args: &[OsString]) -> String {
    let mut line = quote_windows_arg(program.as_os_str());
    for (i, arg) in args.iter().enumerate() {
        line.push(' ');
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
        let trimmed = text.trim_end_matches(['\\', '/']);
        if trimmed.len() == 2 && trimmed.as_bytes().get(1) == Some(&b':') {
            break;
        }
        out.push(parent.to_path_buf());
        cur = parent;
    }
    out
}

/// Open a path for DACL RMW without following reparse points (junction-safe).
#[cfg(windows)]
fn open_path_for_dacl(path: &Path) -> Result<windows::Win32::Foundation::HANDLE, SandboxError> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, READ_CONTROL, WRITE_DAC,
    };

    let path_w: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path_w.as_ptr()),
            READ_CONTROL.0 | WRITE_DAC.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(|err| SandboxError::Backend {
        label: "appcontainer".into(),
        backend: "appcontainer",
        detail: format!(
            "CreateFileW(reparse-safe) for {} failed: {err}",
            path.display()
        ),
    })?;
    if handle.is_invalid() {
        return Err(SandboxError::Backend {
            label: "appcontainer".into(),
            backend: "appcontainer",
            detail: format!("CreateFileW returned invalid handle for {}", path.display()),
        });
    }
    Ok(handle)
}

/// Grant traverse/list on a directory with **no** inheritance.
#[cfg(windows)]
fn grant_directory_traverse_no_inherit(package_sid: &str, path: &Path) -> Result<(), SandboxError> {
    use std::ptr;

    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertStringSidToSidW, GetSecurityInfo, SetEntriesInAclW, SetSecurityInfo,
        EXPLICIT_ACCESS_W, GRANT_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_SID,
        TRUSTEE_IS_WELL_KNOWN_GROUP, TRUSTEE_W,
    };
    use windows::Win32::Security::{ACE_FLAGS, ACL, DACL_SECURITY_INFORMATION, PSID};
    use windows::Win32::Storage::FileSystem::FILE_GENERIC_READ;

    if matches!(classify_acl_path(path), AclPathClass::AmbientOsRuntime) {
        return Err(SandboxError::Backend {
            label: "appcontainer".into(),
            backend: "appcontainer",
            detail: format!(
                "refusing traverse ACL grant under OS-managed path {}",
                path.display()
            ),
        });
    }

    let _lock = acl_api_lock();
    let sid_w: Vec<u16> = package_sid
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let handle = open_path_for_dacl(path)?;

    unsafe {
        let mut psid = PSID(ptr::null_mut());
        if let Err(err) = ConvertStringSidToSidW(PCWSTR(sid_w.as_ptr()), &mut psid) {
            let _ = CloseHandle(handle);
            return Err(SandboxError::Backend {
                label: "appcontainer".into(),
                backend: "appcontainer",
                detail: format!("ConvertStringSidToSidW failed: {err}"),
            });
        }

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
        let st = GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut p_dacl),
            None,
            Some(&mut p_sd),
        );
        if st.0 != 0 {
            let _ = LocalFree(Some(HLOCAL(psid.0)));
            let _ = CloseHandle(handle);
            return Err(SandboxError::Backend {
                label: "appcontainer".into(),
                backend: "appcontainer",
                detail: format!("GetSecurityInfo({}) failed: {st:?}", path.display()),
            });
        }

        let mut new_dacl: *mut ACL = ptr::null_mut();
        let st2 = SetEntriesInAclW(Some(&[ea]), Some(p_dacl as *const ACL), &mut new_dacl);
        if st2.0 != 0 {
            let _ = LocalFree(Some(HLOCAL(p_sd.0)));
            let _ = LocalFree(Some(HLOCAL(psid.0)));
            let _ = CloseHandle(handle);
            return Err(SandboxError::Backend {
                label: "appcontainer".into(),
                backend: "appcontainer",
                detail: format!("SetEntriesInAclW({}) failed: {st2:?}", path.display()),
            });
        }

        let st3 = SetSecurityInfo(
            handle,
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
        let _ = CloseHandle(handle);
        if st3.0 != 0 {
            return Err(SandboxError::Backend {
                label: "appcontainer".into(),
                backend: "appcontainer",
                detail: format!(
                    "SetSecurityInfo(traverse {}) failed: {st3:?}",
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

#[cfg(windows)]
fn grant_package_access(package_sid: &str, path: &Path, write: bool) -> Result<(), SandboxError> {
    use rappct::acl::{grant_to_package, AccessMask, ResourcePath};
    use rappct::AppContainerSid;

    match classify_acl_path(path) {
        AclPathClass::AmbientOsRuntime if write => {
            return Err(SandboxError::Backend {
                label: "appcontainer".to_string(),
                backend: "appcontainer",
                detail: format!(
                    "refusing ACL write grant under OS-managed path {}",
                    path.display()
                ),
            });
        }
        AclPathClass::AmbientOsRuntime => {
            return Err(SandboxError::Backend {
                label: "appcontainer".to_string(),
                backend: "appcontainer",
                detail: format!(
                    "internal error: ACL mutation requested for ambient OS path {}",
                    path.display()
                ),
            });
        }
        AclPathClass::Explicit => {}
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
    let _lock = acl_api_lock();
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
    use windows::Win32::Foundation::{CloseHandle, LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertStringSidToSidW, GetSecurityInfo, SetEntriesInAclW, SetSecurityInfo,
        EXPLICIT_ACCESS_W, REVOKE_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_SID,
        TRUSTEE_IS_WELL_KNOWN_GROUP, TRUSTEE_W,
    };
    use windows::Win32::Security::{ACE_FLAGS, ACL, DACL_SECURITY_INFORMATION, PSID};

    let _lock = acl_api_lock();
    let sid_wide: Vec<u16> = package_sid
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let handle = open_path_for_dacl(path)?;
    let mut psid = PSID(ptr::null_mut());
    unsafe {
        if let Err(err) = ConvertStringSidToSidW(PCWSTR(sid_wide.as_ptr()), &mut psid) {
            let _ = CloseHandle(handle);
            return Err(SandboxError::Backend {
                label: "appcontainer".to_string(),
                backend: "appcontainer",
                detail: format!("ConvertStringSidToSidW failed: {err}"),
            });
        }

        let mut p_sd = windows::Win32::Security::PSECURITY_DESCRIPTOR(ptr::null_mut());
        let mut p_dacl: *mut ACL = ptr::null_mut();
        let st = GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut p_dacl),
            None,
            Some(&mut p_sd),
        );
        if st.0 != 0 {
            let _ = LocalFree(Some(HLOCAL(psid.0)));
            let _ = CloseHandle(handle);
            return Err(SandboxError::Backend {
                label: "appcontainer".to_string(),
                backend: "appcontainer",
                detail: format!("GetSecurityInfo(REVOKE) failed: {st:?}"),
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
            let _ = CloseHandle(handle);
            return Err(SandboxError::Backend {
                label: "appcontainer".to_string(),
                backend: "appcontainer",
                detail: format!("SetEntriesInAclW(REVOKE) failed: {st2:?}"),
            });
        }

        let st3 = SetSecurityInfo(
            handle,
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
        let _ = CloseHandle(handle);
        if st3.0 != 0 {
            return Err(SandboxError::Backend {
                label: "appcontainer".to_string(),
                backend: "appcontainer",
                detail: format!("SetSecurityInfo(REVOKE) failed: {st3:?}"),
            });
        }
    }
    Ok(())
}

/// Return whether `package_sid` still appears in the DACL SDDL for `path`.
///
/// Used by integration tests to prove temporary ACEs are cleaned up.
#[cfg(windows)]
pub fn dacl_mentions_sid(path: &Path, package_sid: &str) -> Result<bool, SandboxError> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW,
        SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows::Win32::Security::{ACL, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};

    let path_w: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let mut p_sd = PSECURITY_DESCRIPTOR(ptr::null_mut());
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
            return Err(SandboxError::Backend {
                label: "appcontainer".into(),
                backend: "appcontainer",
                detail: format!("GetNamedSecurityInfoW failed: {st:?}"),
            });
        }
        let mut sddl = PWSTR::null();
        let mut sddl_len = 0u32;
        let ok = ConvertSecurityDescriptorToStringSecurityDescriptorW(
            p_sd,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut sddl,
            Some(&mut sddl_len),
        );
        let _ = LocalFree(Some(HLOCAL(p_sd.0)));
        if ok.is_err() || sddl.0.is_null() {
            return Err(SandboxError::Backend {
                label: "appcontainer".into(),
                backend: "appcontainer",
                detail: "ConvertSecurityDescriptorToStringSecurityDescriptorW failed".into(),
            });
        }
        // Length is in characters and includes the trailing NUL.
        let len = sddl_len.saturating_sub(1) as usize;
        let slice = std::slice::from_raw_parts(sddl.0, len);
        let text = String::from_utf16_lossy(slice);
        let _ = LocalFree(Some(HLOCAL(sddl.0.cast())));
        Ok(text
            .to_ascii_lowercase()
            .contains(&package_sid.to_ascii_lowercase()))
    }
}

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Policy;

    #[test]
    fn deny_maps_to_no_network_caps() {
        let plan = plan_appcontainer(&Policy::new("t").net(NetPolicy::Deny));
        assert!(plan.capability_names.is_empty());
        assert_eq!(plan.label_stem, "t");
    }

    #[test]
    fn outbound_maps_to_internet_client() {
        let plan = plan_appcontainer(&Policy::new("t").net(NetPolicy::Outbound));
        assert_eq!(plan.capability_names, ["internetClient"]);
    }

    #[test]
    fn outbound_listen_adds_private_network() {
        let plan = plan_appcontainer(&Policy::new("t").net(NetPolicy::OutboundListen));
        assert_eq!(
            plan.capability_names,
            ["internetClient", "privateNetworkClientServer"]
        );
    }

    #[test]
    fn plan_is_pure_and_does_not_require_windows() {
        // Must succeed on every host — no CreateAppContainerProfile.
        let plan = plan_appcontainer(&Policy::new("plugin:libro"));
        assert_eq!(plan.label_stem, "plugin.libro");
    }

    #[test]
    fn profile_stem_sanitizes_plugin_labels() {
        assert_eq!(profile_name_for_label("plugin:libro"), "plugin.libro");
        assert_eq!(profile_name_for_label("!!!"), "guest");
    }

    #[test]
    fn unique_monikers_differ_for_the_same_label() {
        let a = unique_profile_moniker("media-worker:fixup");
        let b = unique_profile_moniker("media-worker:fixup");
        assert_ne!(a, b);
        assert!(a.len() <= 64, "{a}");
        assert!(b.len() <= 64, "{b}");
        assert!(a.starts_with("bc."));
    }

    #[test]
    fn long_labels_that_share_a_64_char_prefix_still_get_distinct_monikers() {
        let shared = "y".repeat(80);
        let a = unique_profile_moniker(&format!("{shared}-one"));
        let b = unique_profile_moniker(&format!("{shared}-two"));
        assert_ne!(a, b);
        assert!(a.len() <= 64);
        assert!(b.len() <= 64);
    }

    #[test]
    fn appcontainer_folder_validation_accepts_measured_and_doc_layouts() {
        let packages = std::path::PathBuf::from(r"C:\Users\me\AppData\Local\Packages");
        let sid = "S-1-15-2-1-2-3-4-5-6-7";
        let moniker_ac = packages.join("bc.example").join("AC");
        assert!(path_is_safe_appcontainer_folder(
            &moniker_ac,
            "bc.example",
            sid,
            Some(packages.as_path())
        ));
        let sid_path = packages.join(sid);
        assert!(path_is_safe_appcontainer_folder(
            &sid_path,
            "bc.example",
            sid,
            Some(packages.as_path())
        ));
        let wrong_sid = packages.join("S-1-15-2-9-9-9-9-9-9-9");
        assert!(!path_is_safe_appcontainer_folder(
            &wrong_sid,
            "bc.example",
            sid,
            Some(packages.as_path())
        ));
        let nested = packages.join("evil").join("Packages").join(sid);
        assert!(!path_is_safe_appcontainer_folder(
            &nested,
            "bc.example",
            sid,
            Some(packages.as_path())
        ));
        let outside = std::path::PathBuf::from(r"C:\Temp\Packages").join(sid);
        assert!(!path_is_safe_appcontainer_folder(
            &outside,
            "bc.example",
            sid,
            Some(packages.as_path())
        ));
    }
}
