//! Nested `NetPolicy::Deny` jail for the native-behind-workerd guest.
//!
//! The workerd launcher itself needs loopback TCP for the RPC bridge, so the
//! host jail stays `OutboundListen`. The native child must not inherit ambient
//! `AF_INET`/`AF_INET6`; it talks Cap'n Proto over stdio and TCP through the
//! Unix socket proxy.

#![allow(clippy::missing_docs_in_private_items)]

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use bookclerk_sandbox::{Enforcement, NetPolicy, Spec, SPEC_ENV};
use tokio::process::Command;

const JAIL_BIN: &str = "bookclerk-jail";
const JAIL_BIN_ENV: &str = "BOOKCLERK_PLUGIN_JAIL";
/// Host sets this to `1` when the outer plugin jail is confined.
const NESTED_JAIL_ENV: &str = "BOOKCLERK_NESTED_NATIVE_JAIL";

/// Builds a command that execs `backend` under `bookclerk-jail` with Deny net
/// when the host requested a nested jail and the helper is available.
///
/// `inherit_fds` are extra descriptors the nested jail must keep (the socket
/// proxy directory fd so the guest can `connect(/proc/self/fd/N/sockets.sock)`).
/// Isolation::Off still inherits them via cleared `FD_CLOEXEC`.
///
/// # Errors
///
/// Returns an error when the nested jail spec cannot be serialized.
pub fn native_guest_command(
    backend: &Path,
    plugin_root: &Path,
    state_dir: &Path,
    inherit_fds: &[i32],
) -> Result<Command> {
    let nested_requested = std::env::var_os(NESTED_JAIL_ENV).is_some_and(|v| v == "1");
    let mut cmd = if nested_requested {
        if let Some(jail) = find_jail() {
            let spec = deny_spec(backend, plugin_root, state_dir, inherit_fds);
            let json = serde_json::to_string(&spec).context("serialize nested native jail spec")?;
            let mut wrapped = Command::new(jail);
            wrapped.env(SPEC_ENV, json);
            wrapped.arg(backend);
            wrapped
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

fn deny_spec(backend: &Path, plugin_root: &Path, state_dir: &Path, inherit_fds: &[i32]) -> Spec {
    let enforcement = if std::env::var_os("BOOKCLERK_SANDBOX_REQUIRE_ENFORCEMENT").is_some() {
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
    spec.net = NetPolicy::Deny;
    spec.allow_exec = true;
    spec.system_paths = true;
    spec.enforcement = enforcement;
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
    use bookclerk_sandbox::NetPolicy;

    #[test]
    fn nested_native_guest_spec_denies_ambient_inet() {
        let root = std::env::temp_dir();
        let spec = deny_spec(&root.join("guest"), &root, &root, &[]);
        assert_eq!(spec.net, NetPolicy::Deny);
        assert!(spec.allow_exec);
        assert_eq!(spec.preserve_fds, vec![0, 1, 2]);
        assert!(spec.writes.iter().any(|p| p == &root));
    }

    #[test]
    fn nested_spec_preserves_socket_proxy_dir_fd() {
        let root = std::env::temp_dir();
        let spec = deny_spec(&root.join("guest"), &root, &root, &[7]);
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
}
