//! Linux backend: Landlock filesystem allowlist plus a seccomp-bpf filter.
//!
//! Both mechanisms are unprivileged. Neither needs user namespaces, which are
//! restricted by default on Ubuntu 23.10+ and so cannot be relied on.

#![allow(unsafe_code)] // prctl and the Landlock ABI probe are raw syscalls.

use landlock::{
    path_beneath_rules, Access, AccessFs, AccessNet, CompatLevel, Compatible, RestrictionStatus,
    Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus, Scope, ABI,
};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

use crate::{Capabilities, LayerStatus, NetPolicy, Policy, Report, SandboxError};

/// Backend name reported in diagnostics.
pub const BACKEND: &str = "landlock+seccomp";

/// Highest Landlock ABI whose semantics we have tested against.
///
/// Requesting a level the kernel does not have is safe — the ruleset is built
/// with [`CompatLevel::BestEffort`] and the result is checked — but pinning a
/// vetted ceiling keeps a future kernel from silently applying access rights we
/// have never exercised.
const VETTED_ABI: ABI = ABI::V6;

/// Read-only paths every Linux process needs: the loader, shared libraries, the
/// CA bundle, and resolver configuration.
///
/// `/proc` is deliberately absent. A same-uid process that can read
/// `/proc/<pid>/environ` or `/proc/<pid>/fd` defeats the environment scrub and
/// can reach the daemon's open database handle, so only `/proc/self` is
/// granted.
pub fn system_read_paths() -> &'static [&'static str] {
    &[
        "/usr",
        "/lib",
        "/lib64",
        "/lib32",
        "/bin",
        "/sbin",
        "/etc/ssl",
        "/etc/ca-certificates",
        "/etc/ca-certificates.conf",
        "/etc/pki",
        "/etc/resolv.conf",
        "/etc/hosts",
        "/etc/nsswitch.conf",
        "/etc/localtime",
        "/etc/ld.so.cache",
        "/etc/ld.so.conf",
        "/etc/ld.so.conf.d",
        "/dev/null",
        "/dev/zero",
        "/dev/urandom",
        "/dev/random",
        "/dev/full",
        "/proc/self",
        "/sys/devices/system/cpu",
    ]
}

/// Query the kernel's Landlock ABI level.
///
/// Returns `Ok(0)` when Landlock is compiled in but disabled, and `Err` when
/// the syscall is missing entirely.
fn probe_abi() -> Result<i32, i32> {
    // landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)
    const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
    let rc = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0_usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if rc < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(-1))
    } else {
        Ok(i32::try_from(rc).unwrap_or(i32::MAX))
    }
}

pub fn capabilities() -> Capabilities {
    let (filesystem, detail) = match probe_abi() {
        Ok(abi) if abi >= 1 => (true, format!("landlock ABI v{abi}")),
        Ok(_) => (false, "landlock present but reports ABI v0".to_string()),
        Err(errno) => (
            false,
            format!("landlock unavailable (landlock_create_ruleset errno {errno})"),
        ),
    };
    Capabilities {
        backend: BACKEND,
        filesystem,
        // seccomp-bpf has been unconditional on supported kernels for a decade.
        syscall: true,
        network: filesystem,
        detail,
    }
}

pub fn confine_current_process(policy: &Policy) -> Result<Report, SandboxError> {
    // Resolve before touching the kernel so a bad path fails loudly first.
    let reads = policy.resolved_reads();
    let writes = policy.resolved_writes();

    let (filesystem, landlock_network) = apply_landlock(policy, &reads, &writes)?;
    let syscall = apply_seccomp(policy)?;

    let network = combine_network_status(policy.net_policy(), landlock_network);

    Ok(Report {
        label: policy.label().to_string(),
        backend: BACKEND,
        filesystem,
        syscall,
        network,
    })
}

/// Fold Landlock's network result together with what seccomp covers.
///
/// The two mechanisms split the work: Landlock can refuse a TCP `bind` but has
/// no way to express "no sockets at all", so `Deny` is carried entirely by the
/// seccomp filter on `socket(2)`. This is only called once that filter has
/// been applied, so `Deny` is enforced by definition here.
fn combine_network_status(net: NetPolicy, from_landlock: LayerStatus) -> LayerStatus {
    match net {
        NetPolicy::Full => LayerStatus::NotRequested,
        NetPolicy::Deny => LayerStatus::Enforced,
        NetPolicy::Outbound => from_landlock,
    }
}

/// Apply the Landlock ruleset.
///
/// Returns the filesystem status and Landlock's contribution to the network
/// status, which only covers [`NetPolicy::Outbound`]. The caller combines it
/// with the seccomp result.
fn apply_landlock(
    policy: &Policy,
    reads: &[std::path::PathBuf],
    writes: &[std::path::PathBuf],
) -> Result<(LayerStatus, LayerStatus), SandboxError> {
    let abi = VETTED_ABI;

    // Landlock can restrict binding but not connecting-in-general, so it covers
    // `Outbound` (refuse listeners). `Deny` is enforced by seccomp instead,
    // because it must also cover UDP and raw sockets.
    let restrict_bind = policy.net_policy() == NetPolicy::Outbound;

    let status: RestrictionStatus = (|| -> Result<RestrictionStatus, landlock::RulesetError> {
        let mut ruleset = Ruleset::default().set_compatibility(CompatLevel::BestEffort);
        ruleset = ruleset.handle_access(AccessFs::from_all(abi))?;
        if restrict_bind {
            ruleset = ruleset.handle_access(AccessNet::BindTcp)?;
        }
        // Deny signalling and abstract-socket connections to processes outside
        // this domain (ABI 6+). Downgrades to a no-op on older kernels.
        ruleset = ruleset.scope(Scope::from_all(abi))?;

        let mut created = ruleset.create()?;
        created = created.add_rules(path_beneath_rules(reads, AccessFs::from_read(abi)))?;
        created = created.add_rules(path_beneath_rules(writes, AccessFs::from_all(abi)))?;
        // No `NetPort` rules are added, so handling `BindTcp` above refuses
        // every bind while leaving outbound connections untouched.
        created.restrict_self()
    })()
    .map_err(|err| SandboxError::backend(policy.label(), err))?;

    let filesystem = match status.ruleset {
        RulesetStatus::FullyEnforced => LayerStatus::Enforced,
        RulesetStatus::PartiallyEnforced => LayerStatus::Partial(format!(
            "kernel landlock ABI below v{}; some access rights unhandled",
            abi as i32
        )),
        // The bug PR #61 shipped: treating this as success left the process
        // completely unconfined while reporting a jail.
        RulesetStatus::NotEnforced => LayerStatus::Unsupported(
            "kernel has no usable Landlock support; no filesystem allowlist applied".to_string(),
        ),
    };

    let network = match (policy.net_policy(), &filesystem) {
        (NetPolicy::Full | NetPolicy::Deny, _) => LayerStatus::NotRequested,
        (NetPolicy::Outbound, LayerStatus::Enforced) => LayerStatus::Enforced,
        (NetPolicy::Outbound, _) => LayerStatus::Unsupported(
            "landlock could not restrict TCP bind; inbound listeners are not blocked".to_string(),
        ),
    };

    Ok((filesystem, network))
}

/// Syscalls no Bookclerk component has any business making. Denied with
/// `EPERM` rather than killing the process, so a library probing for a feature
/// degrades instead of crashing.
fn denied_syscalls() -> Vec<i64> {
    let mut denied = vec![
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_kexec_load,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_reboot,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_userfaultfd,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_open_by_handle_at,
        libc::SYS_setns,
        libc::SYS_unshare,
        libc::SYS_acct,
        libc::SYS_settimeofday,
        libc::SYS_clock_settime,
        libc::SYS_syslog,
        libc::SYS_setuid,
        libc::SYS_setgid,
        libc::SYS_setreuid,
        libc::SYS_setregid,
        libc::SYS_setresuid,
        libc::SYS_setresgid,
    ];
    #[cfg(target_arch = "x86_64")]
    {
        denied.push(libc::SYS_kexec_file_load);
        denied.push(libc::SYS_ioperm);
        denied.push(libc::SYS_iopl);
        denied.push(libc::SYS_modify_ldt);
    }
    denied
}

fn apply_seccomp(policy: &Policy) -> Result<LayerStatus, SandboxError> {
    let arch: TargetArch = std::env::consts::ARCH.try_into().map_err(|_| {
        SandboxError::backend(policy.label(), "unsupported architecture for seccomp")
    })?;

    let mut rules: Vec<(i64, Vec<SeccompRule>)> = denied_syscalls()
        .into_iter()
        .map(|nr| (nr, vec![]))
        .collect();

    // `NetPolicy::Deny` means no IP sockets of any kind. Filtering `socket` by
    // address family also covers UDP and raw, which Landlock's TCP-only network
    // rules cannot reach.
    if policy.net_policy() == NetPolicy::Deny {
        let mut socket_rules = Vec::new();
        for family in [libc::AF_INET, libc::AF_INET6, libc::AF_PACKET] {
            let condition =
                SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, family as u64)
                    .map_err(|err| SandboxError::backend(policy.label(), err))?;
            socket_rules.push(
                SeccompRule::new(vec![condition])
                    .map_err(|err| SandboxError::backend(policy.label(), err))?,
            );
        }
        rules.push((libc::SYS_socket, socket_rules));
    }

    if !policy.exec_allowed() {
        rules.push((libc::SYS_execve, vec![]));
        rules.push((libc::SYS_execveat, vec![]));
    }

    // Landlock's `restrict_self` already sets it, but seccomp must not depend
    // on Landlock having succeeded.
    let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if rc != 0 {
        return Err(SandboxError::backend(
            policy.label(),
            format!(
                "PR_SET_NO_NEW_PRIVS failed: {}",
                std::io::Error::last_os_error()
            ),
        ));
    }

    let filter = SeccompFilter::new(
        rules.into_iter().collect(),
        // Default: allow. This is a deny list, not an allow list — the
        // filesystem allowlist is the primary boundary, and an allow list over
        // tokio plus three codec libraries would break on every dependency bump.
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )
    .map_err(|err| SandboxError::backend(policy.label(), err))?;

    let program: BpfProgram = filter
        .try_into()
        .map_err(|err| SandboxError::backend(policy.label(), format!("{err:?}")))?;
    seccompiler::apply_filter(&program)
        .map_err(|err| SandboxError::backend(policy.label(), err))?;

    Ok(LayerStatus::Enforced)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_probe_reports_a_definite_answer() {
        // Either the syscall exists and returns a version, or it does not.
        // Both are fine; a panic or a hang is not.
        match probe_abi() {
            Ok(abi) => assert!(abi >= 0),
            Err(errno) => assert!(errno != 0),
        }
    }

    #[test]
    fn capabilities_detail_mentions_landlock() {
        let caps = capabilities();
        assert_eq!(caps.backend, BACKEND);
        assert!(caps.detail.contains("landlock"), "detail: {}", caps.detail);
        assert!(caps.syscall, "seccomp should always be reported available");
    }

    /// `Deny` is enforced by seccomp, not Landlock, and used to be reported as
    /// "not requested" — which reads like the restriction never engaged. The
    /// media worker runs with `Deny`, so that is the line operators see most.
    #[test]
    fn deny_reports_as_enforced_even_though_landlock_did_not_do_it() {
        assert_eq!(
            combine_network_status(NetPolicy::Deny, LayerStatus::NotRequested),
            LayerStatus::Enforced
        );
    }

    #[test]
    fn outbound_reports_what_landlock_managed() {
        assert_eq!(
            combine_network_status(NetPolicy::Outbound, LayerStatus::Enforced),
            LayerStatus::Enforced
        );
        let unsupported = LayerStatus::Unsupported("no bind restriction".into());
        assert_eq!(
            combine_network_status(NetPolicy::Outbound, unsupported.clone()),
            unsupported
        );
    }

    #[test]
    fn full_network_is_not_a_restriction() {
        assert_eq!(
            combine_network_status(NetPolicy::Full, LayerStatus::NotRequested),
            LayerStatus::NotRequested
        );
    }

    #[test]
    fn deny_list_covers_ptrace_and_module_loading() {
        let denied = denied_syscalls();
        assert!(denied.contains(&libc::SYS_ptrace));
        assert!(denied.contains(&libc::SYS_init_module));
        assert!(denied.contains(&libc::SYS_setuid));
    }
}
