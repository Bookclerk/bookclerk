//! Linux backend: Landlock filesystem allowlist plus a seccomp-bpf filter.
//!
//! Both mechanisms are unprivileged. Neither needs user namespaces, which are
//! restricted by default on Ubuntu 23.10+ and so cannot be relied on.

#![allow(unsafe_code)] // prctl and the Landlock ABI probe are raw syscalls.

use std::path::Path;

use landlock::{
    path_beneath_rules, Access, AccessFs, AccessNet, CompatLevel, Compatible, NetPort,
    RestrictionStatus, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus, Scope, ABI,
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
/// `/etc/ssl` alone is not enough on openSUSE/SLE: `ca-bundle.pem` and `certs/`
/// are symlinks into `/var/lib/ca-certificates`, and Landlock evaluates the
/// symlink target. Without that directory, `reqwest`/`rustls` fail at
/// `Client::build()` with a opaque "builder error", which breaks Discover
/// catalog search (and any other HTTPS) inside the guest jail.
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
        // openSUSE / SLE (and some others): real CA bundle lives here.
        "/var/lib/ca-certificates",
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

/// Paths from the system set that must be writable, not just readable.
///
/// Only `/dev/null` qualifies. Discarding output by redirecting to it is so
/// ordinary — a shell redirect, a library silencing a logger — that a read-only
/// grant reads as a broken jail rather than as policy. The other character
/// devices in the read set stay read-only because nothing writes to them on
/// purpose.
pub fn system_write_paths() -> &'static [&'static str] {
    &["/dev/null"]
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

/// Reports Landlock ABI, seccomp, and whether filesystem/network confinement is available.
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
        // Guests are confined by self-confine + exec in bookclerk-jail.
        spawn_filesystem: false,
        // seccomp-bpf has been unconditional on supported kernels for a decade.
        syscall: true,
        network: filesystem,
        detail,
    }
}

/// Applies cgroup ceilings, Landlock, and seccomp to this process; fails closed on FS/net.
pub fn confine_current_process(policy: &Policy) -> Result<Report, SandboxError> {
    // Resolve before touching the kernel so a bad path fails loudly first.
    let reads = policy.resolved_reads();
    let writes = policy.resolved_writes();

    // Resource ceilings before Landlock/seccomp. Failure is NotApplicable
    // (best-effort); Required still hinges on FS/net, not on a writable cgroup.
    let resources = apply_cgroup_v2_limits(policy);

    let (filesystem, landlock_network) = apply_landlock(policy, &reads, &writes)?;
    let syscall = apply_seccomp(policy)?;

    let network = combine_network_status(policy.net_policy(), landlock_network);

    Ok(Report {
        label: policy.label().to_string(),
        backend: BACKEND,
        filesystem,
        syscall,
        network,
        resources,
    })
}

/// Best-effort cgroup v2 `memory.max` / `cpu.max` / `pids.max` when Spec asked.
///
/// Returns [`LayerStatus::NotRequested`] when no resource fields are set (native
/// guests keep prior behavior). A missing or unwritable hierarchy is
/// [`NotApplicable`] — same posture as macOS Seatbelt: we never claim
/// `Enforced`, and [`Enforcement::Required`] still passes on FS/net. Callers that
/// demand resource enforcement for CI use dedicated tests / env knobs rather
/// than failing every Required guest when the host cannot delegate a leaf.
fn apply_cgroup_v2_limits(policy: &Policy) -> LayerStatus {
    if !policy.has_resource_limits() {
        return LayerStatus::NotRequested;
    }
    let limits = policy.resource_limits();
    match try_apply_cgroup_v2(&limits, policy.cgroup_dir_opt()) {
        Ok(()) => LayerStatus::Enforced,
        Err(detail) => {
            let detail = friendly_cgroup_error(detail);
            if cgroup_controllers_not_delegated(&detail) {
                tracing::debug!(
                    label = %policy.label(),
                    error = %detail,
                    "cgroup v2 resource limits not applied"
                );
            } else {
                tracing::warn!(
                    label = %policy.label(),
                    error = %detail,
                    "cgroup v2 resource limits not applied"
                );
            }
            LayerStatus::NotApplicable(detail)
        }
    }
}

/// Try to place this process into a **child** cgroup with the requested ceilings.
///
/// Never writes limits onto the current/parent cgroup: that would throttle
/// siblings (and in CI, the whole job) sharing the runner slice.
///
/// When `join` is set (host-created session leaf), this process moves into
/// that directory and does **not** rewrite limits — the host already wrote
/// the aggregate memory/CPU/pids ceilings.
fn try_apply_cgroup_v2(limits: &crate::ResourceLimits, join: Option<&Path>) -> Result<(), String> {
    if let Some(dir) = join {
        if !dir.is_dir() {
            return Err(format!("cgroup_dir {} is not a directory", dir.display()));
        }
        return move_self_into_cgroup(dir);
    }

    let root = Path::new("/sys/fs/cgroup");
    if !root.join("cgroup.controllers").is_file() {
        return Err("cgroup v2 not mounted at /sys/fs/cgroup".into());
    }

    let current_rel = current_cgroup_v2_path()?;
    let parent = if current_rel.is_empty() || current_rel == "/" {
        root.to_path_buf()
    } else {
        root.join(current_rel.trim_start_matches('/'))
    };
    if !parent.is_dir() {
        return Err(format!(
            "current cgroup path {} is missing under /sys/fs/cgroup",
            parent.display()
        ));
    }

    // Prefer a dedicated leaf so we do not fight "no internal processes".
    let child_name = format!("bookclerk-{}", std::process::id());
    let child = parent.join(&child_name);

    // Enable controllers on the parent when possible (may fail if parent still
    // has processes — then child create fails closed below).
    let _ = enable_subtree_controllers(&parent);

    match std::fs::create_dir(&child) {
        Ok(()) => apply_limits_in_child(&child, limits),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            apply_limits_in_child(&child, limits)
        }
        Err(create_err) => Err(format!(
            "could not create child cgroup {}: {create_err} \
             (refusing to write limits onto shared parent {})",
            child.display(),
            parent.display()
        )),
    }
}

/// Writes Spec ceilings into `child` and moves this PID there; removes an empty
/// child directory when the write fails so leftover `bookclerk-<pid>` nodes
/// do not accumulate under an undelegateable parent slice.
fn apply_limits_in_child(child: &Path, limits: &crate::ResourceLimits) -> Result<(), String> {
    match write_cgroup_limits(child, limits).and_then(|()| move_self_into_cgroup(child)) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_dir(child);
            Err(err)
        }
    }
}

/// Rewrites EPERM/EACCES cgroup writes into an operator-facing explanation.
///
/// Desktop app slices (browser/IDE cgroups) often allow creating a child
/// directory but not enabling `memory`/`cpu`/`pids` on `subtree_control`.
/// Filesystem/syscall/network jail still apply; resource caps are best-effort.
fn friendly_cgroup_error(detail: String) -> String {
    if cgroup_permission_denied(&detail) {
        "cgroup resource controllers are not delegated in this slice \
         (common under desktop app scopes); filesystem/syscall/network jail still apply"
            .into()
    } else {
        detail
    }
}

/// True when `detail` is the rewritten "controllers not delegated" message.
fn cgroup_controllers_not_delegated(detail: &str) -> bool {
    detail.contains("not delegated in this slice")
}

/// True when a cgroup sysfs write failed with EACCES/EPERM.
fn cgroup_permission_denied(detail: &str) -> bool {
    detail.contains("Permission denied") || detail.contains("Operation not permitted")
}

/// Reads this process's cgroup v2 path from `/proc/self/cgroup` (`0::…`).
fn current_cgroup_v2_path() -> Result<String, String> {
    let raw = std::fs::read_to_string("/proc/self/cgroup")
        .map_err(|err| format!("read /proc/self/cgroup: {err}"))?;
    for line in raw.lines() {
        // v2: `0::/user.slice/...`
        if let Some(path) = line.strip_prefix("0::") {
            return Ok(path.to_string());
        }
    }
    Err("no cgroup v2 entry in /proc/self/cgroup".into())
}

/// Controllers a session leaf wants. `pids` is required for `pids.max`.
const SESSION_CONTROLLERS: [&str; 3] = ["memory", "cpu", "pids"];

/// Enables `memory`/`cpu`/`pids` on the parent cgroup when those controllers exist.
fn enable_subtree_controllers(parent: &Path) -> Result<(), String> {
    let available = std::fs::read_to_string(parent.join("cgroup.controllers"))
        .map_err(|err| format!("read cgroup.controllers: {err}"))?;
    let mut enable = String::new();
    for name in SESSION_CONTROLLERS {
        if available.split_whitespace().any(|c| c == name) {
            if !enable.is_empty() {
                enable.push(' ');
            }
            enable.push('+');
            enable.push_str(name);
        }
    }
    if enable.is_empty() {
        return Err("memory/cpu/pids controllers unavailable".into());
    }
    std::fs::write(parent.join("cgroup.subtree_control"), &enable)
        .map_err(|err| format!("write cgroup.subtree_control ({enable}): {err}"))
}

/// True when `dir/cgroup.procs` lists at least one member.
fn cgroup_populated(dir: &Path) -> bool {
    match std::fs::read_to_string(dir.join("cgroup.procs")) {
        Ok(text) => text.split_whitespace().any(|line| !line.is_empty()),
        Err(_) => true,
    }
}

/// True when `name` is already distributed to children of `dir`.
fn subtree_has_controller(dir: &Path, name: &str) -> bool {
    std::fs::read_to_string(dir.join("cgroup.subtree_control"))
        .map(|text| text.split_whitespace().any(|token| token == name))
        .unwrap_or(false)
}

/// Enables each session controller that `dir` has but does not yet distribute.
///
/// One write per controller: a refused domain controller (`memory`, `cpu` on a
/// populated cgroup) must not block `pids`.
fn enable_session_controllers(dir: &Path) {
    let Ok(available) = std::fs::read_to_string(dir.join("cgroup.controllers")) else {
        return;
    };
    let available: Vec<&str> = available.split_whitespace().collect();
    for name in SESSION_CONTROLLERS {
        if !available.contains(&name) || subtree_has_controller(dir, name) {
            continue;
        }
        let _ = std::fs::write(dir.join("cgroup.subtree_control"), format!("+{name}"));
    }
}

/// Parent directory for a session leaf.
///
/// A populated cgroup cannot enable domain controllers in `subtree_control`
/// (the no-internal-process rule). Enabling `pids` there turns the cgroup into
/// a threaded domain and its children into `domain invalid`, which cannot take
/// members. Walk up to the nearest ancestor that has no member processes — or
/// the root, which is exempt — enable the controllers there, and require
/// `pids` in that ancestor's `subtree_control`.
fn nearest_delegating_parent(current: &Path) -> std::path::PathBuf {
    let root = Path::new("/sys/fs/cgroup");
    let mut cursor = current.to_path_buf();
    loop {
        let exempt = cursor == root;
        if exempt || !cgroup_populated(&cursor) {
            enable_session_controllers(&cursor);
            if subtree_has_controller(&cursor, "pids") {
                return cursor;
            }
        }
        if exempt {
            break;
        }
        match cursor.parent() {
            Some(parent) if parent.starts_with(root) => {
                cursor = parent.to_path_buf();
            }
            _ => break,
        }
    }
    current.to_path_buf()
}

/// Writes one controller file.
///
/// On cgroupfs a missing file means that controller was not delegated.
/// [`Ok`]`(false)` records that skip. A temp directory is not cgroupfs, so the
/// write creates the file (unit tests). A present file that cannot be written
/// is an error.
fn write_controller_file(dir: &Path, name: &str, value: &str) -> Result<bool, String> {
    let path = dir.join(name);
    if path.starts_with("/sys/fs/cgroup") && !path.exists() {
        return Ok(false);
    }
    std::fs::write(&path, value).map_err(|err| format!("write {}: {err}", path.display()))?;
    Ok(true)
}

/// Writes `memory.max`, `cpu.max` (100 ms period), and `pids.max` when the policy set them.
///
/// A controller file that does not exist on cgroupfs is skipped. `pids.max` is
/// not optional when `active_processes` is set: the leaf is refused rather than
/// kept without the thread cap. Other skipped controllers are warned and the
/// files that do exist are still written.
fn write_cgroup_limits(dir: &Path, limits: &crate::ResourceLimits) -> Result<(), String> {
    let mut skipped = Vec::new();
    if let Some(bytes) = limits.memory_bytes {
        if !write_controller_file(dir, "memory.max", &bytes.to_string())? {
            skipped.push("memory");
        }
    }
    if let Some(percent) = limits.cpu_rate_percent {
        // cgroup v2 cpu.max: "$MAX $PERIOD" in microseconds. 100ms period;
        // percent is of **one** logical CPU (100 → one core; 200 → two cores).
        const PERIOD_US: u64 = 100_000;
        let pct = u64::from(percent.max(1));
        let quota = PERIOD_US.saturating_mul(pct) / 100;
        if !write_controller_file(dir, "cpu.max", &format!("{quota} {PERIOD_US}"))? {
            skipped.push("cpu");
        }
    }
    if let Some(n) = limits.active_processes {
        if !write_controller_file(dir, "pids.max", &n.to_string())? {
            return Err(
                "pids controller is not delegated; refusing a session leaf without pids.max".into(),
            );
        }
    }
    if !skipped.is_empty() {
        tracing::warn!(
            skipped = %skipped.join(","),
            dir = %dir.display(),
            "cgroup controller not delegated; the remaining limits were applied"
        );
    }
    Ok(())
}

/// Moves this PID into `dir/cgroup.procs` so limits apply only to this leaf.
fn move_self_into_cgroup(dir: &Path) -> Result<(), String> {
    let path = dir.join("cgroup.procs");
    let pid = std::process::id().to_string();
    std::fs::write(&path, &pid).map_err(|err| format!("move pid into {}: {err}", path.display()))
}

/// Create an exclusive session cgroup leaf and write `limits` without moving the caller.
///
/// The host assigns both sibling jails via [`crate::Spec::cgroup_dir`]. The leaf
/// name is the suffix the caller supplies; `create_dir` fails when that name
/// already exists so two sessions never share a leaf. `pids.max` is the thread
/// budget in `limits.active_processes` (not the Windows process baseline).
///
/// The leaf is created under [`nearest_delegating_parent`], which may be an
/// ancestor of this process. A populated cgroup cannot distribute domain
/// controllers to children. The leaf is not created in that cgroup.
///
/// Failure is best-effort (same posture as `try_apply_cgroup_v2`): callers treat
/// `Err` as not-applicable and fall back to process-group kill, which does not
/// cover a descendant that calls `setsid`. That is not fake enforcement.
///
/// # Errors
///
/// Returns a string when the hierarchy is missing, the leaf already exists, a
/// child cannot be created, `pids` cannot be delegated, or a present limit file
/// cannot be written. A partial directory is removed when writing limits fails.
pub fn create_session_cgroup(
    limits: &crate::ResourceLimits,
    suffix: &str,
) -> Result<std::path::PathBuf, String> {
    let root = Path::new("/sys/fs/cgroup");
    if !root.join("cgroup.controllers").is_file() {
        return Err("cgroup v2 not mounted at /sys/fs/cgroup".into());
    }
    let current_rel = current_cgroup_v2_path()?;
    let current = if current_rel.is_empty() || current_rel == "/" {
        root.to_path_buf()
    } else {
        root.join(current_rel.trim_start_matches('/'))
    };
    if !current.is_dir() {
        return Err(format!(
            "current cgroup path {} is missing under /sys/fs/cgroup",
            current.display()
        ));
    }
    let parent = nearest_delegating_parent(&current);
    if !subtree_has_controller(&parent, "pids") {
        return Err(format!(
            "pids controller is not delegated under {} (current cgroup {})",
            parent.display(),
            current.display()
        ));
    }
    if parent != current {
        tracing::debug!(
            parent = %parent.display(),
            current = %current.display(),
            "session cgroup parent is an ancestor that can delegate controllers"
        );
    }
    let suffix = suffix
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect::<String>();
    if suffix.is_empty() {
        return Err("session cgroup suffix is empty".into());
    }
    let child_name = format!("bookclerk-session-{suffix}");
    let child = parent.join(&child_name);
    match std::fs::create_dir(&child) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(format!(
                "session cgroup {} already exists; refusing to share a leaf",
                child.display()
            ));
        }
        Err(err) => {
            return Err(format!(
                "could not create session cgroup {}: {err}",
                child.display()
            ));
        }
    }
    if let Err(err) = write_cgroup_limits(&child, limits) {
        let _ = std::fs::remove_dir(&child);
        return Err(err);
    }
    Ok(child)
}

/// Kill every member of `dir` except this process, wait until `cgroup.procs`
/// is empty, and remove the leaf.
///
/// A missing directory is success, so teardown is idempotent. Pid 0 is never
/// signalled. The host process is skipped when it appears in the leaf.
///
/// # Errors
///
/// Returns a string when membership cannot be read, members are still present
/// after the wait, or the empty directory cannot be removed.
pub fn destroy_session_cgroup(dir: &Path) -> Result<(), String> {
    if !dir.exists() {
        return Ok(());
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let host = std::process::id();
    loop {
        let members = read_cgroup_procs(dir)?;
        let mut alive = false;
        for pid in members {
            if pid == 0 || pid == host {
                continue;
            }
            alive = true;
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
        if !alive {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "session cgroup {} still has members after kill",
                dir.display()
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    match std::fs::remove_dir(dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("remove session cgroup {}: {err}", dir.display())),
    }
}

/// Pids listed in `dir/cgroup.procs`. A missing file is an empty set.
fn read_cgroup_procs(dir: &Path) -> Result<Vec<u32>, String> {
    let path = dir.join("cgroup.procs");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("read {}: {err}", path.display())),
    };
    let mut pids = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let pid = line
            .parse::<u32>()
            .map_err(|err| format!("cgroup.procs pid `{line}`: {err}"))?;
        pids.push(pid);
    }
    Ok(pids)
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
        NetPolicy::Outbound | NetPolicy::OutboundListen => from_landlock,
    }
}

/// Whether a policy needs Landlock to handle `bind`.
///
/// Landlock is the only backend with per-port bind rules, so this question is
/// Landlock's rather than the policy's. `Full` asks for no restriction and `Deny`
/// is carried by refusing sockets outright, so neither needs a bind rule.
fn restricts_bind(net: NetPolicy) -> bool {
    matches!(net, NetPolicy::Outbound | NetPolicy::OutboundListen)
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
    // the two outbound policies. `Deny` is enforced by seccomp instead, because
    // it must also cover UDP and raw sockets.
    let restrict_bind = restricts_bind(policy.net_policy());

    let status: RestrictionStatus = (|| -> Result<RestrictionStatus, landlock::RulesetError> {
        let mut ruleset = Ruleset::default().set_compatibility(CompatLevel::BestEffort);
        ruleset = ruleset.handle_access(AccessFs::from_all(abi))?;
        if restrict_bind {
            ruleset = ruleset.handle_access(AccessNet::BindTcp)?;
        }
        // Deny signalling and abstract-socket connections to processes outside
        // this domain (ABI 6+). Downgrades to a no-op on older kernels.
        // Nested native-behind-workerd guests therefore cannot use a parent
        // `abstract:` SOCKET_PROXY; the launcher advertises a pathname under
        // `/proc/self/fd/{inherited dir fd}/` instead (see bookclerk-workerd
        // `unix_bind::bind_socket_proxy`).
        ruleset = ruleset.scope(Scope::from_all(abi))?;

        let mut created = ruleset.create()?;
        created = created.add_rules(path_beneath_rules(reads, AccessFs::from_read(abi)))?;
        created = created.add_rules(path_beneath_rules(writes, AccessFs::from_all(abi)))?;
        // Port 0 is the ABI's spelling for "whatever the kernel hands out from
        // `ip_local_port_range`". Adding it is what separates `OutboundListen`
        // from `Outbound`, which adds no rule and so refuses every bind.
        if policy.net_policy() == NetPolicy::OutboundListen {
            created = created.add_rule(NetPort::new(0, AccessNet::BindTcp))?;
        }
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
        (NetPolicy::Outbound | NetPolicy::OutboundListen, LayerStatus::Enforced) => {
            LayerStatus::Enforced
        }
        (NetPolicy::Outbound | NetPolicy::OutboundListen, _) => LayerStatus::Unsupported(
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

/// Installs a seccomp-bpf filter denying privileged syscalls and, when asked, IP sockets.
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

    /// openSUSE/SLE symlink the CA bundle under `/etc/ssl` into this directory;
    /// Landlock checks the target, so HTTPS guests need an explicit grant.
    #[test]
    fn system_reads_include_var_lib_ca_certificates() {
        let paths = system_read_paths();
        assert!(
            paths.contains(&"/var/lib/ca-certificates"),
            "missing /var/lib/ca-certificates in {paths:?}"
        );
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

    /// `OutboundListen` is still a bind restriction, so it reports whatever
    /// Landlock managed rather than falling through to "not requested".
    #[test]
    fn outbound_listen_reports_what_landlock_managed() {
        assert_eq!(
            combine_network_status(NetPolicy::OutboundListen, LayerStatus::Enforced),
            LayerStatus::Enforced
        );
        let unsupported = LayerStatus::Unsupported("no bind restriction".into());
        assert_eq!(
            combine_network_status(NetPolicy::OutboundListen, unsupported.clone()),
            unsupported
        );
    }

    #[test]
    fn only_the_outbound_policies_add_bind_rules() {
        assert!(restricts_bind(NetPolicy::Outbound));
        assert!(restricts_bind(NetPolicy::OutboundListen));
        assert!(!restricts_bind(NetPolicy::Deny));
        assert!(!restricts_bind(NetPolicy::Full));
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

    #[test]
    fn cgroup_limit_files_use_expected_wire_format() {
        let dir = tempfile::tempdir().expect("tempdir");
        let limits = crate::ResourceLimits {
            memory_bytes: Some(512 * 1024 * 1024),
            cpu_rate_percent: Some(80),
            active_processes: Some(8),
        };
        write_cgroup_limits(dir.path(), &limits).expect("write");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("memory.max")).unwrap(),
            (512 * 1024 * 1024).to_string()
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("cpu.max")).unwrap(),
            "80000 100000"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("pids.max")).unwrap(),
            "8"
        );
    }

    #[test]
    fn joining_a_session_cgroup_does_not_rewrite_limits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let limits = crate::ResourceLimits {
            memory_bytes: Some(64 * 1024 * 1024),
            cpu_rate_percent: Some(25),
            active_processes: Some(3),
        };
        // A missing join target fails closed.
        let missing = dir.path().join("nope");
        let err = try_apply_cgroup_v2(&limits, Some(&missing)).expect_err("missing");
        assert!(err.contains("not a directory"), "{err}");

        // A present leaf is joined by writing cgroup.procs only — never
        // memory.max / cpu.max / pids.max (those stay host-owned).
        try_apply_cgroup_v2(&limits, Some(dir.path())).expect("join temp leaf");
        assert!(
            !dir.path().join("memory.max").exists(),
            "join must not rewrite host-owned limits"
        );
        assert!(!dir.path().join("cpu.max").exists());
        assert!(!dir.path().join("pids.max").exists());
        let procs = std::fs::read_to_string(dir.path().join("cgroup.procs")).expect("procs");
        assert_eq!(procs, std::process::id().to_string());
    }

    /// Real delegated hierarchy only. A temp file named `cgroup.procs` is not
    /// this test: uniqueness is `create_dir` failing with `AlreadyExists`.
    #[test]
    fn exclusive_session_leaves_when_the_hierarchy_is_delegated() {
        let limits = crate::ResourceLimits {
            memory_bytes: None,
            cpu_rate_percent: None,
            active_processes: Some(32),
        };
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let suffix = format!("excl-{}-{nonce}", std::process::id());
        let first = match create_session_cgroup(&limits, &suffix) {
            Ok(dir) => dir,
            Err(err) => {
                eprintln!(
                    "delegated cgroup unavailable ({err}); exclusive leaf not enforced here. \
                     process-group kill is the fallback and does not cover a descendant that calls setsid"
                );
                return;
            }
        };
        let err = create_session_cgroup(&limits, &suffix)
            .expect_err("a second session must not share the leaf");
        assert!(
            err.contains("already exists"),
            "collision must name the existing leaf: {err}"
        );
        let second =
            create_session_cgroup(&limits, &format!("{suffix}-b")).expect("distinct suffix");
        assert_ne!(
            first, second,
            "two sessions of one plugin need distinct leaves"
        );
        destroy_session_cgroup(&first).expect("remove first");
        destroy_session_cgroup(&second).expect("remove second");
        assert!(!first.exists());
        assert!(!second.exists());
        destroy_session_cgroup(&first).expect("idempotent destroy");
    }

    #[test]
    fn resource_limits_absent_report_not_requested() {
        assert_eq!(
            apply_cgroup_v2_limits(&Policy::new("plugin:echo")),
            LayerStatus::NotRequested
        );
    }

    #[test]
    fn permission_denied_cgroup_write_is_rewritten() {
        let raw =
            "write /sys/fs/cgroup/user.slice/app.slice/memory.max: Permission denied (os error 13)";
        let friendly = friendly_cgroup_error(raw.into());
        assert!(cgroup_controllers_not_delegated(&friendly), "{friendly}");
        assert!(
            !friendly.contains("Permission denied"),
            "raw sysfs path must not leak into the jail summary: {friendly}"
        );
    }
}
