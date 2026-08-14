//! macOS backend: Seatbelt via `sandbox_init`.
//!
//! `sandbox_init` has carried a deprecation attribute since 10.8, but Apple has
//! published no replacement that works for binaries outside a signed, entitled
//! app bundle. It remains the mechanism Chrome, Bazel, Nix, and Homebrew ship,
//! and the kernel enforcement behind it is not deprecated. App Sandbox
//! entitlements plus an `SMAppService` XPC helper would be the supported path
//! once Bookclerk ships a signed bundle; that is not the case today.
//!
//! The profile here is deliberately broader than the Linux Landlock policy.
//! dyld, Mach bootstrap, and the system trust store need enough read access
//! that a true deny-default filesystem view will not launch a process.

#![allow(unsafe_code)] // sandbox_init is a C entry point.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::path::{Path, PathBuf};

use crate::{Capabilities, LayerStatus, NetPolicy, Policy, Report, SandboxError};

/// Backend name reported in diagnostics.
pub const BACKEND: &str = "seatbelt";

#[link(name = "sandbox")]
unsafe extern "C" {
    fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> c_int;
    fn sandbox_free_error(errorbuf: *mut c_char);
}

/// Read-only paths a macOS process needs to load and resolve names.
pub fn system_read_paths() -> &'static [&'static str] {
    &[
        "/usr",
        "/System",
        "/Library",
        "/bin",
        "/sbin",
        "/dev",
        "/etc",
        "/private/etc",
        "/private/var/db/mds",
        "/private/var/select",
        "/opt/homebrew",
        "/opt/local",
        "/usr/local",
    ]
}

/// Paths from the system set that must be writable, not just readable.
///
/// See the Linux backend for why `/dev/null` is the only entry.
pub fn system_write_paths() -> &'static [&'static str] {
    &["/dev/null"]
}

/// Reports which Seatbelt layers this backend can enforce for the current host.
pub fn capabilities() -> Capabilities {
    Capabilities {
        backend: BACKEND,
        filesystem: true,
        // Guests are confined by self-confine + exec in bookclerk-jail.
        spawn_filesystem: false,
        // Seatbelt gates operations, not syscall numbers. There is no seccomp
        // equivalent, so we do not claim one.
        syscall: false,
        network: true,
        detail: "seatbelt sandbox_init (deprecated by Apple, no replacement for \
                 non-bundled binaries)"
            .to_string(),
    }
}

/// Applies the Seatbelt profile for `policy` to the current process.
///
/// # Errors
///
/// Returns [`SandboxError`] when the profile cannot be encoded or `sandbox_init`
/// rejects it.
pub fn confine_current_process(policy: &Policy) -> Result<Report, SandboxError> {
    let profile = build_profile(policy);
    let c_profile = CString::new(profile)
        .map_err(|_| SandboxError::backend(policy.label(), "profile contains an interior NUL"))?;

    let mut err: *mut c_char = std::ptr::null_mut();
    let rc = unsafe { sandbox_init(c_profile.as_ptr(), 0, &mut err) };
    if rc != 0 {
        let detail = if err.is_null() {
            "sandbox_init failed without an error message".to_string()
        } else {
            let message = unsafe { CStr::from_ptr(err) }
                .to_string_lossy()
                .into_owned();
            unsafe { sandbox_free_error(err) };
            message
        };
        return Err(SandboxError::backend(policy.label(), detail));
    }

    let network = match policy.net_policy() {
        NetPolicy::Full => LayerStatus::NotRequested,
        NetPolicy::Deny | NetPolicy::Outbound | NetPolicy::OutboundListen => LayerStatus::Enforced,
    };

    Ok(Report {
        label: policy.label().to_string(),
        backend: BACKEND,
        filesystem: LayerStatus::Partial(
            "seatbelt profile is broader than the Linux policy (dyld and Mach \
             bootstrap require it)"
                .to_string(),
        ),
        // Not a gap: the profile is `(deny default)`, so the operations the
        // Linux seccomp list exists to block — `exec`, sockets, ptrace — are
        // already refused unless explicitly allowed above. There is simply no
        // syscall-number filter to report.
        syscall: LayerStatus::NotApplicable(
            "seatbelt gates operation classes, not syscall numbers; (deny default) \
             already refuses exec and unlisted operations"
                .to_string(),
        ),
        network,
        // Seatbelt has no memory / CPU rate / pids controls. Not a silent gap:
        // we never claim Enforced. Required still passes (like syscall).
        resources: if policy.has_resource_limits() {
            LayerStatus::NotApplicable(
                "macOS Seatbelt enforces filesystem/network only; memory/CPU/pids \
                 are unsupported (no fake enforcement)"
                    .into(),
            )
        } else {
            LayerStatus::NotRequested
        },
    })
}

/// Builds the Seatbelt profile text for `policy` (deny-default SBPL).
fn build_profile(policy: &Policy) -> String {
    let mut out = String::from("(version 1)\n(deny default)\n");

    // Process and IPC primitives the Rust runtime and dyld need.
    out.push_str("(allow process-fork)\n");
    out.push_str("(allow signal (target self))\n");
    out.push_str("(allow sysctl-read)\n");
    out.push_str("(allow mach-lookup)\n");
    out.push_str("(allow mach-register)\n");
    out.push_str("(allow system-socket)\n");
    out.push_str("(allow file-read-metadata)\n");
    out.push_str("(allow file-ioctl (subpath \"/dev\"))\n");

    let reads = policy.resolved_reads();
    let writes = policy.resolved_writes();

    if policy.exec_allowed() {
        out.push_str("(allow process-exec)\n");
        // Mapping an executable image is a distinct operation from reading the
        // file, and `(deny default)` refuses it. Reading `/bin/sh` is therefore
        // not enough to run it: the kernel maps the image and the dyld shared
        // cache before any user code exists to report a problem, so the exec'd
        // program dies silently instead of returning EPERM. That is invisible to
        // a process that finished its own dyld work *before* confining itself,
        // which is why only the launcher's hand-off hits it.
        //
        // Scoped to the paths already readable, so this grants no new visibility
        // — only the right to treat what is readable as an executable image.
        push_paths(&mut out, "file-map-executable", &reads, &writes);
        // dyld still needs to list "/" to locate the shared cache before the
        // handoff target runs; without this the exec'd program SIGABRTs silently.
        out.push_str("(allow file-read-data (literal \"/\"))\n");
    }

    match policy.net_policy() {
        NetPolicy::Deny => {}
        NetPolicy::Outbound => out.push_str("(allow network-outbound)\n"),
        // Seatbelt filters by address where Landlock filters by port, so the
        // listener is pinned to loopback here rather than to the ephemeral
        // range. An OAuth callback server only ever wants loopback.
        NetPolicy::OutboundListen => {
            out.push_str("(allow network-outbound)\n");
            out.push_str("(allow network-bind (local ip \"localhost:*\"))\n");
            out.push_str("(allow network-inbound (local ip \"localhost:*\"))\n");
        }
        NetPolicy::Full => {
            out.push_str("(allow network-outbound)\n");
            out.push_str("(allow network-inbound)\n");
            out.push_str("(allow network-bind)\n");
        }
    }

    push_paths(&mut out, "file-read*", &reads, &[]);
    // Writable paths must also be readable; SBPL treats the two separately.
    push_paths(&mut out, "file-read* file-write*", &writes, &[]);

    out
}

/// Emit `(allow <operations> …paths)`, or nothing when no path qualifies.
///
/// An operation with no filter allows it everywhere, so an empty allowlist must
/// produce no rule at all rather than an unfiltered grant.
fn push_paths(out: &mut String, operations: &str, first: &[PathBuf], second: &[PathBuf]) {
    if first.is_empty() && second.is_empty() {
        return;
    }
    out.push_str("(allow ");
    out.push_str(operations);
    out.push('\n');
    for path in first.iter().chain(second) {
        push_path(out, path);
    }
    out.push_str(")\n");
}

/// Emit a filter for one allowlist entry.
///
/// `subpath` is a prefix match, which is what a directory needs. A single file
/// gets `literal` so the rule cannot also match a sibling whose name merely
/// starts with the same characters.
fn push_path(out: &mut String, path: &Path) {
    let filter = if path.is_dir() { "subpath" } else { "literal" };
    out.push_str("  (");
    out.push_str(filter);
    out.push_str(" \"");
    out.push_str(&escape_sbpl(&path.display().to_string()));
    out.push_str("\")\n");
}

/// Escape a path for an SBPL string literal.
///
/// Backslashes and quotes are escaped. Characters that would terminate or
/// restructure the s-expression are dropped rather than escaped, because SBPL
/// has no escape for them and a partially-parsed profile is worse than a path
/// that fails to match.
fn escape_sbpl(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for ch in path.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' | '\r' | '(' | ')' | ';' => {}
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The path filters of one `(allow …)` rule.
    ///
    /// Path lines end in `")` , so the rule's own closing paren is the only one
    /// sitting alone on a line.
    fn rule_body<'a>(profile: &'a str, operations: &str) -> &'a str {
        let marker = format!("(allow {operations}\n");
        let start = profile
            .find(&marker)
            .unwrap_or_else(|| panic!("no {operations} rule in:\n{profile}"))
            + marker.len();
        let body = &profile[start..];
        &body[..body.find("\n)\n").expect("rule should close")]
    }

    #[test]
    fn profile_is_deny_default() {
        let profile = build_profile(&Policy::new("test").system_paths(false));
        assert!(profile.starts_with("(version 1)\n(deny default)\n"));
    }

    #[test]
    fn deny_net_omits_network_rules() {
        let profile = build_profile(&Policy::new("test").system_paths(false));
        assert!(!profile.contains("network-outbound"));
    }

    #[test]
    fn outbound_net_allows_outbound_only() {
        let profile = build_profile(
            &Policy::new("test")
                .system_paths(false)
                .net(NetPolicy::Outbound),
        );
        assert!(profile.contains("(allow network-outbound)"));
        assert!(!profile.contains("network-inbound"));
    }

    /// The callback listener must be reachable from loopback and nowhere else,
    /// so both bind and inbound carry a `local ip` filter.
    #[test]
    fn outbound_listen_pins_the_listener_to_loopback() {
        let profile = build_profile(
            &Policy::new("test")
                .system_paths(false)
                .net(NetPolicy::OutboundListen),
        );
        assert!(profile.contains("(allow network-outbound)"), "{profile}");
        assert!(
            profile.contains("(allow network-bind (local ip \"localhost:*\"))"),
            "{profile}"
        );
        assert!(
            profile.contains("(allow network-inbound (local ip \"localhost:*\"))"),
            "{profile}"
        );
        // An unfiltered grant would make this indistinguishable from `Full`.
        assert!(!profile.contains("(allow network-bind)\n"), "{profile}");
    }

    #[test]
    fn writable_paths_are_also_readable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let profile = build_profile(&Policy::new("test").system_paths(false).write(dir.path()));
        assert!(profile.contains("(allow file-read* file-write*"));
    }

    #[test]
    fn directories_get_subpath_and_files_get_literal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("book.m4b");
        std::fs::write(&file, b"audio").expect("write fixture");

        let profile = build_profile(
            &Policy::new("test")
                .system_paths(false)
                .read(&file)
                .write(dir.path()),
        );
        assert!(profile.contains("(literal \""), "{profile}");
        assert!(profile.contains("(subpath \""), "{profile}");
    }

    /// `TMPDIR` on macOS is under `/var/folders`, and `/var` is a symlink to
    /// `/private/var`. Seatbelt matches physical paths, so a rule written
    /// against the `/var` spelling matches nothing and the job fails with
    /// EPERM despite a report that says the jail engaged.
    #[test]
    fn profile_paths_are_physical() {
        let dir = tempfile::tempdir().expect("tempdir");
        let profile = build_profile(&Policy::new("test").system_paths(false).write(dir.path()));
        let physical = std::fs::canonicalize(dir.path()).expect("canonicalize");
        assert!(
            profile.contains(&physical.display().to_string()),
            "profile should name the physical path {}: {profile}",
            physical.display()
        );
    }

    #[test]
    fn escape_removes_expression_breakers() {
        assert_eq!(escape_sbpl("/tmp/a(b)c"), "/tmp/abc");
        assert_eq!(escape_sbpl("/tmp/a\"b"), "/tmp/a\\\"b");
        assert_eq!(escape_sbpl("/tmp/a\\b"), "/tmp/a\\\\b");
    }

    #[test]
    fn exec_is_opt_in() {
        let without = build_profile(&Policy::new("test").system_paths(false));
        assert!(!without.contains("process-exec"));
        let with = build_profile(&Policy::new("test").system_paths(false).allow_exec(true));
        assert!(with.contains("(allow process-exec)"));
    }

    /// Reading a program is not enough to run it: the kernel maps the image
    /// before any user code exists, so a missing `file-map-executable` kills the
    /// exec'd program with no diagnostic at all. The grant must cover the system
    /// set, since that is where `/bin/sh` and the dyld cache live.
    #[test]
    fn exec_also_grants_mapping_the_images_it_will_run() {
        let profile = build_profile(&Policy::new("test").allow_exec(true));
        let mapping = rule_body(&profile, "file-map-executable");
        for needed in ["/usr", "/bin"] {
            assert!(
                mapping.contains(&format!("\"{needed}\"")),
                "{needed} should be mappable: {mapping}"
            );
        }
    }

    /// Without exec there is nothing to map, and an unscoped grant would be the
    /// widest rule in the profile.
    #[test]
    fn mapping_is_not_granted_without_exec() {
        let profile = build_profile(&Policy::new("test"));
        assert!(!profile.contains("file-map-executable"), "{profile}");
    }

    #[test]
    fn exec_grants_reading_the_root_directory_and_not_its_contents() {
        let profile = build_profile(&Policy::new("test").allow_exec(true));
        assert!(
            profile.contains("(allow file-read-data (literal \"/\"))"),
            "{profile}"
        );
        assert!(!profile.contains("(subpath \"/\")"), "{profile}");
    }

    /// An operation with no path filter allows it everywhere, which would turn
    /// an empty allowlist into the opposite of what it says.
    #[test]
    fn an_empty_allowlist_emits_no_rule_rather_than_an_unfiltered_one() {
        let profile = build_profile(&Policy::new("test").system_paths(false).allow_exec(true));
        assert!(!profile.contains("file-read*"), "{profile}");
        assert!(!profile.contains("file-write*"), "{profile}");
        assert!(!profile.contains("file-map-executable"), "{profile}");
    }
}
