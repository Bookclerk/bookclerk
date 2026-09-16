//! Nested `NetPolicy::Deny` jail for the native-behind-workerd guest.
//!
//! The workerd launcher itself needs loopback TCP for the RPC bridge, so the
//! host jail stays `OutboundListen`. The native child must not inherit ambient
//! `AF_INET`/`AF_INET6`; it talks Cap'n Proto over stdio and TCP through the
//! host socket proxy (Unix domain socket or Windows named pipe).

#![allow(clippy::missing_docs_in_private_items)]

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use bookclerk_sandbox::{Enforcement, NetPolicy, Spec, SPEC_ENV};
use tokio::process::Command;

const JAIL_BIN: &str = "bookclerk-jail";
const JAIL_BIN_ENV: &str = "BOOKCLERK_PLUGIN_JAIL";
/// Host sets this to `1` for native-behind-workerd guests so ambient `AF_INET`
/// is denied even when the outer launcher jail is Isolation::Off.
const NESTED_JAIL_ENV: &str = "BOOKCLERK_NESTED_NATIVE_JAIL";
/// When `required`, nested jail creation/launch failures fail closed.
const NESTED_JAIL_ENFORCEMENT_ENV: &str = "BOOKCLERK_NESTED_JAIL_ENFORCEMENT";
/// Pre-created nested AppContainer profile moniker (Windows).
const NESTED_AC_PROFILE_ENV: &str = "BOOKCLERK_NESTED_AC_PROFILE";
/// Nested AppContainer Package SID used to ACL the SOCKET_PROXY pipe (Windows).
#[cfg(windows)]
pub const NESTED_AC_SID_ENV: &str = "BOOKCLERK_NESTED_AC_SID";

/// Builds a command that execs `backend` under `bookclerk-jail` with Deny net
/// when the host requested a nested jail and the helper is available.
///
/// `inherit_fds` are extra descriptors the nested jail must keep (the socket
/// proxy directory fd so the guest can `connect(/proc/self/fd/N/sockets.sock)`).
/// Isolation::Off still inherits them via cleared `FD_CLOEXEC`.
///
/// # Errors
///
/// Returns an error when nested Deny is required but `bookclerk-jail` is
/// missing, or when the nested jail spec cannot be serialized.
pub fn native_guest_command(
    backend: &Path,
    plugin_root: &Path,
    state_dir: &Path,
    inherit_fds: &[i32],
) -> Result<Command> {
    wrap_native_guest(
        backend,
        plugin_root,
        state_dir,
        inherit_fds,
        std::env::var_os(NESTED_JAIL_ENV).is_some_and(|v| v == "1"),
        find_jail(),
        nested_enforcement_required(),
    )
}

/// True when nested Deny must not be skipped if `bookclerk-jail` is absent.
///
/// Host Isolation::Required sets [`NESTED_JAIL_ENFORCEMENT_ENV`]. CI also
/// sets `BOOKCLERK_SANDBOX_REQUIRE_ENFORCEMENT`, which must not silently
/// exec the backend with ambient `AF_INET`.
fn nested_enforcement_required() -> bool {
    std::env::var_os("BOOKCLERK_SANDBOX_REQUIRE_ENFORCEMENT").is_some()
        || std::env::var(NESTED_JAIL_ENFORCEMENT_ENV).as_deref() == Ok("required")
}

fn wrap_native_guest(
    backend: &Path,
    plugin_root: &Path,
    state_dir: &Path,
    inherit_fds: &[i32],
    nested_requested: bool,
    jail: Option<PathBuf>,
    enforcement_required: bool,
) -> Result<Command> {
    let mut cmd = if nested_requested {
        if let Some(jail) = jail {
            let spec = deny_spec_with(
                backend,
                plugin_root,
                state_dir,
                inherit_fds,
                enforcement_required,
            );
            let json = serde_json::to_string(&spec).context("serialize nested native jail spec")?;
            let mut wrapped = Command::new(jail);
            wrapped.env(SPEC_ENV, json);
            wrapped.arg(backend);
            wrapped
        } else if enforcement_required {
            anyhow::bail!(
                "bookclerk-jail not found beside bookclerk-workerd; nested AF_INET denial is required"
            );
        } else {
            tracing::warn!(
                "bookclerk-jail not found beside bookclerk-workerd; native guest will not get nested AF_INET denial"
            );
            Command::new(backend)
        }
    } else {
        Command::new(backend)
    };
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    Ok(cmd)
}

fn deny_spec_with(
    backend: &Path,
    plugin_root: &Path,
    state_dir: &Path,
    inherit_fds: &[i32],
    enforcement_required: bool,
) -> Spec {
    let enforcement = if enforcement_required {
        Enforcement::Required
    } else {
        Enforcement::BestEffort
    };
    let mut spec = Spec::new(format!("native-behind-workerd:{}", backend.display()));
    spec.reads.push(plugin_root.to_path_buf());
    if let Some(dir) = backend.parent() {
        spec.reads.push(dir.to_path_buf());
    }
    spec.writes.push(state_dir.to_path_buf());
    spec.writes.extend(inherited_write_roots());
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // sqlx Postgres mediation binds `{dir}/.s.PGSQL.{port}` under a short
        // `/tmp/bc-pg-{pid}` path so sockaddr_un cannot overflow.
        spec.writes.push(PathBuf::from("/tmp"));
    }
    spec.net = NetPolicy::Deny;
    spec.allow_exec = true;
    spec.system_paths = true;
    spec.enforcement = enforcement;
    if let Ok(name) = std::env::var(NESTED_AC_PROFILE_ENV) {
        if !name.is_empty() {
            spec.windows_profile_name = Some(name);
        }
    }
    spec.preserve_fds = {
        let mut fds = vec![0, 1, 2];
        fds.extend(inherit_fds.iter().copied().filter(|fd| *fd > 2));
        fds.sort_unstable();
        fds.dedup();
        fds
    };
    spec
}

/// Host-assigned guest trees inherited from `bookclerk-workerd`'s environment.
///
/// Nested Landlock can only tighten the parent jail. HOME / TMPDIR / sqlite
/// library files live outside the workerd state dir and must be re-granted or
/// the adapter cannot open `library.db`.
fn inherited_write_roots() -> Vec<PathBuf> {
    extra_write_roots(
        ["HOME", "TMPDIR", "TEMP", "TMP"]
            .into_iter()
            .filter_map(|key| std::env::var_os(key).map(|v| (key, PathBuf::from(v))))
            .chain(
                std::env::var_os("BOOKCLERK_SQLITE_PATH")
                    .map(|v| ("BOOKCLERK_SQLITE_PATH", PathBuf::from(v))),
            ),
    )
}

fn extra_write_roots(vars: impl IntoIterator<Item = (&'static str, PathBuf)>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for (key, path) in vars {
        if path.as_os_str().is_empty() {
            continue;
        }
        if key == "BOOKCLERK_SQLITE_PATH" {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    roots.push(parent.to_path_buf());
                }
            }
        } else {
            roots.push(path);
        }
    }
    roots.sort();
    roots.dedup();
    roots
}

fn find_jail() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(JAIL_BIN_ENV) {
        let p = PathBuf::from(path);
        if p.is_file() {
            return Some(p);
        }
    }
    let name = format!("{JAIL_BIN}{}", std::env::consts::EXE_SUFFIX);
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(&name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_sandbox::{Enforcement, NetPolicy};

    #[test]
    fn nested_native_guest_spec_denies_ambient_inet() {
        let root = std::env::temp_dir();
        let spec = deny_spec_with(&root.join("guest"), &root, &root, &[], false);
        assert_eq!(spec.net, NetPolicy::Deny);
        assert!(spec.allow_exec);
        assert_eq!(spec.preserve_fds, vec![0, 1, 2]);
        assert!(spec.writes.iter().any(|p| p == &root));
    }

    #[test]
    fn nested_spec_preserves_socket_proxy_dir_fd() {
        let root = std::env::temp_dir();
        let spec = deny_spec_with(&root.join("guest"), &root, &root, &[7], false);
        assert_eq!(spec.preserve_fds, vec![0, 1, 2, 7]);
    }

    #[test]
    fn extra_write_roots_regrant_sqlite_library_parent() {
        let files = PathBuf::from("/tmp/bookclerk-files");
        let roots = extra_write_roots([
            ("HOME", files.join("plugins/pk-sqlite/data")),
            ("TMPDIR", files.join("plugins/pk-sqlite/tmp")),
            ("BOOKCLERK_SQLITE_PATH", files.join("library.db")),
        ]);
        assert!(roots.contains(&files));
        assert!(roots.iter().any(|p| p.ends_with("data")));
        assert!(roots.iter().any(|p| p.ends_with("tmp")));
    }

    #[test]
    fn nested_deny_spec_is_required_when_enforcement_is_required() {
        let root = std::env::temp_dir();
        let spec = deny_spec_with(&root.join("guest"), &root, &root, &[], true);
        assert_eq!(spec.enforcement, Enforcement::Required);
        assert_eq!(spec.net, NetPolicy::Deny);
    }

    #[test]
    fn missing_jail_fail_closes_when_nested_enforcement_required() {
        let root = std::env::temp_dir();
        let err = wrap_native_guest(&root.join("guest"), &root, &root, &[], true, None, true)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("bookclerk-jail not found"),
            "missing jail must fail closed when nested Deny is required: {err}"
        );
    }

    #[test]
    fn missing_jail_continues_when_nested_enforcement_is_best_effort() {
        let root = std::env::temp_dir();
        wrap_native_guest(&root.join("guest"), &root, &root, &[], true, None, false)
            .expect("Isolation::Off may continue without bookclerk-jail");
    }
}
