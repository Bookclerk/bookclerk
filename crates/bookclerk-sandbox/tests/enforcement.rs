//! End-to-end proof that the jail actually holds.
//!
//! Confinement is irreversible and process-wide, so each case re-executes this
//! test binary as a child helper, confines the child, and asserts on what the
//! child could reach. A test that only inspects the [`Report`] would pass just
//! as happily against a sandbox that never engaged — which is precisely the bug
//! that shipped in the first attempt at this feature.
//!
//! Each helper exits `0` only when every expectation held, so a silently
//! unconfined child fails the test rather than passing it.

use std::path::Path;
use std::process::Command;

use bookclerk_sandbox::{Enforcement, NetPolicy, Policy};

/// Env var naming which helper the re-executed child should run.
const ROLE: &str = "BOOKCLERK_SANDBOX_TEST_ROLE";
/// Directory the child is allowed to read and write.
const ALLOWED: &str = "BOOKCLERK_SANDBOX_TEST_ALLOWED";
/// File outside the allowlist that the child must not be able to read.
const DENIED: &str = "BOOKCLERK_SANDBOX_TEST_DENIED";
/// Set on hosts that are expected to enforce, turning a skip into a failure.
const REQUIRE: &str = "BOOKCLERK_SANDBOX_REQUIRE_ENFORCEMENT";

/// Whether [`REQUIRE`] asks this host to prove it can confine.
///
/// An empty value counts as unset, so a CI matrix can clear the variable for
/// platforms where skipping is expected without depending on whether the
/// runner turns `""` into an absent variable.
fn enforcement_demanded() -> bool {
    std::env::var(REQUIRE).is_ok_and(|value| !value.trim().is_empty())
}

/// Whether this host can enforce a filesystem allowlist.
///
/// Every enforcement test is a no-op without a backend, which would let a
/// misconfigured CI runner — or a kernel that quietly lost Landlock — report
/// green while proving nothing. Setting [`REQUIRE`] makes that a failure
/// instead, so the skip is only ever taken where it is genuinely expected.
fn backend_enforces_filesystem() -> bool {
    let caps = bookclerk_sandbox::capabilities();
    assert!(
        caps.filesystem || !enforcement_demanded(),
        "{REQUIRE} is set but this host cannot enforce a filesystem \
         allowlist: {} [{}]",
        caps.detail,
        caps.backend
    );
    caps.filesystem
}

/// Run this test binary again with `ROLE` set, and return its exit status.
fn run_helper(role: &str, allowed: &Path, denied: &Path) -> std::process::Output {
    let exe = std::env::current_exe().expect("current_exe");
    let exe = exe.canonicalize().expect("canonicalize current_exe");
    assert!(exe.is_file(), "current_exe must be a regular file");
    Command::new(exe)
        .arg("--nocapture")
        // Run only the helper entry point, not the whole suite.
        .arg("helper_entry_point")
        .env(ROLE, role)
        .env(ALLOWED, allowed)
        .env(DENIED, denied)
        .output()
        .expect("spawn helper")
}

/// Confine `path` under trusted `root` (CodeQL two-state barrier).
fn under_root(path: &Path, root: &Path) -> Result<std::path::PathBuf, String> {
    bookclerk_sandbox::require_under_root(path, root).map_err(|err| err.to_string())
}

/// The child side. Dispatches on `ROLE`; a normal test run has it unset and
/// this returns immediately.
#[test]
fn helper_entry_point() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    let allowed = std::path::PathBuf::from(std::env::var(ALLOWED).expect("ALLOWED"));
    let denied = std::path::PathBuf::from(std::env::var(DENIED).expect("DENIED"));

    let outcome = match role.as_str() {
        "filesystem" => child_filesystem(&allowed, &denied),
        "network_denied" => child_network_denied(&allowed),
        "network_outbound" => child_network_outbound(&allowed),
        "network_outbound_listen" => child_network_outbound_listen(&allowed),
        "media_worker_shape" => child_media_worker_shape(&allowed, &denied),
        "plugin_guest_shape" => child_plugin_guest_shape(&allowed),
        other => Err(format!("unknown helper role {other}")),
    };

    match outcome {
        Ok(()) => std::process::exit(0),
        Err(err) => {
            eprintln!("helper failure: {err}");
            std::process::exit(1);
        }
    }
}

/// Confine to `allowed`, then verify the allowlist is real in both directions.
fn child_filesystem(allowed: &Path, denied: &Path) -> Result<(), String> {
    let allowed = under_root(allowed, allowed)?;
    let denied_root = denied.parent().unwrap_or(denied);
    let denied = under_root(denied, denied_root)?;

    // Prove the denied path is readable *before* confinement, so a failure after
    // it cannot be blamed on a bad path or missing fixture.
    std::fs::read_to_string(&denied)
        .map_err(|err| format!("denied path unreadable before confinement: {err}"))?;

    let report = Policy::new("test-filesystem")
        .write(&allowed)
        .enforcement(Enforcement::Required)
        .confine_current_process()
        .map_err(|err| format!("confinement failed: {err}"))?;

    if !report.is_confined() {
        return Err(format!("report says unconfined: {}", report.summary()));
    }

    if std::fs::read_to_string(&denied).is_ok() {
        return Err(format!(
            "read the denied path at {} from inside the jail",
            denied.display()
        ));
    }

    // The allowlist must still work, or the jail is useless rather than secure.
    let scratch = under_root(&allowed.join("written-inside-jail.txt"), &allowed)?;
    std::fs::write(&scratch, b"ok").map_err(|err| format!("write inside allowlist: {err}"))?;
    std::fs::read_to_string(&scratch).map_err(|err| format!("read inside allowlist: {err}"))?;

    // Creating a *sibling* of the allowed dir must fail; otherwise the rule was
    // applied to the parent rather than the directory itself.
    // Intentional traversal probe — do not wrap with under_root (it would refuse).
    let escape = allowed.join("..").join("escaped.txt");
    if std::fs::write(&escape, b"nope").is_ok() {
        return Err("wrote outside the allowlist via a parent traversal".to_string());
    }

    // Discarding output has to keep working. A read-only `/dev/null` breaks
    // ordinary shell redirects and any library that silences itself that way.
    std::fs::write("/dev/null", b"discard")
        .map_err(|err| format!("/dev/null is not writable inside the jail: {err}"))?;

    Ok(())
}

/// `NetPolicy::Deny` must refuse IP sockets outright.
fn child_network_denied(allowed: &Path) -> Result<(), String> {
    Policy::new("test-network")
        .write(allowed)
        .net(NetPolicy::Deny)
        .enforcement(Enforcement::Required)
        .confine_current_process()
        .map_err(|err| format!("confinement failed: {err}"))?;

    // Any IP socket at all, not just a successful connection.
    match std::net::TcpStream::connect("127.0.0.1:9") {
        Ok(_) => Err("opened a TCP connection under NetPolicy::Deny".to_string()),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => Ok(()),
        // A refused connection means the socket was created, so the filter did
        // not engage. Connection-refused is a failure here, not a pass.
        Err(err) => Err(format!(
            "expected PermissionDenied from socket(2), got {:?}: {err}",
            err.kind()
        )),
    }
}

/// `NetPolicy::Outbound` must still allow sockets but refuse every listener.
fn child_network_outbound(allowed: &Path) -> Result<(), String> {
    Policy::new("test-network-outbound")
        .write(allowed)
        .net(NetPolicy::Outbound)
        .enforcement(Enforcement::Required)
        .confine_current_process()
        .map_err(|err| format!("confinement failed: {err}"))?;

    // Sockets themselves must still work, or a plugin could not fetch anything.
    // Port 9 (discard) is closed here, so a refusal proves socket(2) succeeded.
    match std::net::TcpStream::connect("127.0.0.1:9") {
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err("outbound connections were blocked under Outbound".to_string())
        }
        _ => {}
    }

    if std::net::TcpListener::bind("127.0.0.1:0").is_ok() {
        return Err("bound a listener under NetPolicy::Outbound".to_string());
    }
    Ok(())
}

/// `NetPolicy::OutboundListen` must allow the OAuth callback listener — a bind
/// on a kernel-assigned port — while still narrowing what can be claimed.
fn child_network_outbound_listen(allowed: &Path) -> Result<(), String> {
    Policy::new("test-network-outbound-listen")
        .write(allowed)
        .net(NetPolicy::OutboundListen)
        .enforcement(Enforcement::Required)
        .confine_current_process()
        .map_err(|err| format!("confinement failed: {err}"))?;

    // This is the bind `audible-rs` performs for the login callback.
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|err| format!("ephemeral bind refused under OutboundListen: {err}"))?;
    let port = listener
        .local_addr()
        .map_err(|err| format!("local_addr: {err}"))?
        .port();
    if port == 0 {
        return Err("kernel assigned port 0".to_string());
    }

    // Landlock's grant is per-port, so a fixed port outside the ephemeral range
    // stays refused. Seatbelt filters by address instead and would allow this,
    // which is why the check is Linux-only.
    #[cfg(target_os = "linux")]
    {
        let fixed = linux_fixed_port_outside_ephemeral();
        if std::net::TcpListener::bind(format!("127.0.0.1:{fixed}")).is_ok() {
            return Err(format!("bound fixed port {fixed} under OutboundListen"));
        }
    }

    Ok(())
}

/// Mirrors the policy `bookclerk-media-worker` builds: read the job's input
/// file, write the job's output directory, no network.
///
/// The worker derives its allowlist from the job, so this is the shape that
/// actually has to hold in production — a media job must not be able to reach
/// `master.key` or `library.db` even though they sit under the same files dir.
fn child_media_worker_shape(job_dir: &Path, files_dir: &Path) -> Result<(), String> {
    let job_dir = under_root(job_dir, job_dir)?;
    let files_dir = under_root(files_dir, files_dir)?;
    let input = under_root(&job_dir.join("book.m4b"), &job_dir)?;
    let output_dir = under_root(&job_dir.join("out"), &job_dir)?;
    std::fs::create_dir_all(&output_dir).map_err(|err| format!("create output dir: {err}"))?;

    let master_key = under_root(&files_dir.join("master.key"), &files_dir)?;
    let library_db = under_root(&files_dir.join("library.db"), &files_dir)?;

    Policy::new("media-worker:encode_mp3")
        .read(&input)
        .write(&output_dir)
        .net(NetPolicy::Deny)
        .allow_exec(false)
        .enforcement(Enforcement::Required)
        .confine_current_process()
        .map_err(|err| format!("confinement failed: {err}"))?;

    for denied in [&master_key, &library_db] {
        if std::fs::read(denied).is_ok() {
            return Err(format!("media job read {}", denied.display()));
        }
    }

    // The job's own paths must still work.
    std::fs::read(&input).map_err(|err| format!("declared input unreadable: {err}"))?;
    let encoded = under_root(&output_dir.join("encoded.mp3"), &output_dir)?;
    std::fs::write(&encoded, b"out")
        .map_err(|err| format!("declared output dir unwritable: {err}"))?;

    // Writing into the files dir must fail even though a sibling is writable.
    let planted = under_root(&files_dir.join("planted"), &files_dir)?;
    if std::fs::write(&planted, b"x").is_ok() {
        return Err("media job wrote into the files dir".to_string());
    }

    Ok(())
}

/// Mirrors nested Landlock/Seatbelt rules: a read-only parent with more
/// specific writable children. Product plugin state is a sibling
/// (`$FILES_DIR/plugin-state/<PluginKey fs-id>/{data,tmp}`), not nested under
/// the install tree; this probe still uses nested `plugins/probe/{data,tmp}`
/// to prove the backend honours the more specific write grant.
fn child_plugin_guest_shape(files_dir: &Path) -> Result<(), String> {
    let files_dir = under_root(files_dir, files_dir)?;
    let install = under_root(&files_dir.join("plugins").join("probe"), &files_dir)?;
    let data = under_root(&install.join("data"), &files_dir)?;
    let scratch = under_root(&install.join("tmp"), &files_dir)?;
    let cache = under_root(&files_dir.join("cache"), &files_dir)?;
    for dir in [&data, &scratch, &cache] {
        std::fs::create_dir_all(dir).map_err(|err| format!("create {}: {err}", dir.display()))?;
    }

    Policy::new("plugin:probe")
        .read(&install)
        .write(&data)
        .write(&scratch)
        .write(&cache)
        .net(NetPolicy::Outbound)
        // The launcher execs the guest, so a plugin jail always permits this.
        .allow_exec(true)
        .enforcement(Enforcement::Required)
        .confine_current_process()
        .map_err(|err| format!("confinement failed: {err}"))?;

    for denied in [
        under_root(&files_dir.join("master.key"), &files_dir)?,
        under_root(&files_dir.join("library.db"), &files_dir)?,
    ] {
        if std::fs::read(&denied).is_ok() {
            return Err(format!("guest read {}", denied.display()));
        }
    }

    // A guest reads its own manifest and binary, and must not be able to rewrite
    // either — the next start would read them back.
    let manifest = under_root(&install.join("plugin.toml"), &files_dir)?;
    std::fs::read(&manifest).map_err(|err| format!("own manifest unreadable: {err}"))?;
    if std::fs::write(&manifest, b"id = \"other\"").is_ok() {
        return Err("guest rewrote its own manifest".to_string());
    }

    for writable in [
        under_root(&data.join("state"), &files_dir)?,
        under_root(&scratch.join("job"), &files_dir)?,
        under_root(&cache.join("part"), &files_dir)?,
    ] {
        std::fs::write(&writable, b"ok")
            .map_err(|err| format!("{} unwritable: {err}", writable.display()))?;
    }

    let planted = under_root(&files_dir.join("planted"), &files_dir)?;
    if std::fs::write(&planted, b"x").is_ok() {
        return Err("guest wrote into the files dir".to_string());
    }
    Ok(())
}

#[test]
fn media_worker_policy_shape_cannot_reach_key_material() {
    if !backend_enforces_filesystem() {
        eprintln!("skipping: no filesystem confinement on this host");
        return;
    }

    let job_dir = tempfile::tempdir().expect("tempdir");
    let files_dir = tempfile::tempdir().expect("tempdir");
    let book =
        bookclerk_sandbox::require_under_root(&job_dir.path().join("book.m4b"), job_dir.path())
            .expect("book under job_dir");
    let key = bookclerk_sandbox::require_under_root(
        &files_dir.path().join("master.key"),
        files_dir.path(),
    )
    .expect("key under files_dir");
    let db = bookclerk_sandbox::require_under_root(
        &files_dir.path().join("library.db"),
        files_dir.path(),
    )
    .expect("db under files_dir");
    std::fs::write(&book, b"fake audio").expect("write input");
    std::fs::write(&key, b"sealed-dek").expect("write key");
    std::fs::write(&db, b"sqlite").expect("write db");

    let output = run_helper("media_worker_shape", job_dir.path(), files_dir.path());
    assert!(
        output.status.success(),
        "helper failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn plugin_guest_policy_shape_writes_only_its_own_directories() {
    if !backend_enforces_filesystem() {
        eprintln!("skipping: no filesystem confinement on this host");
        return;
    }

    let files_dir = tempfile::tempdir().expect("tempdir");
    let install = files_dir.path().join("plugins").join("probe");
    std::fs::create_dir_all(&install).expect("create install dir");
    std::fs::write(install.join("plugin.toml"), b"id = \"probe\"\n").expect("write manifest");
    std::fs::write(files_dir.path().join("master.key"), b"sealed-dek").expect("write key");
    std::fs::write(files_dir.path().join("library.db"), b"sqlite").expect("write db");

    let output = run_helper(
        "plugin_guest_shape",
        files_dir.path(),
        &files_dir.path().join("master.key"),
    );
    assert!(
        output.status.success(),
        "helper failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn filesystem_allowlist_blocks_paths_outside_it() {
    if !backend_enforces_filesystem() {
        eprintln!("skipping: no filesystem confinement on this host");
        return;
    }

    let jail = tempfile::tempdir().expect("tempdir");
    let vault = tempfile::tempdir().expect("tempdir");
    let denied = vault.path().join("master.key");
    std::fs::write(
        &denied,
        [b"pretend-this-is-a-".as_slice(), b"data-encryption-key"].concat(),
    )
    .expect("write denied fixture");

    let output = run_helper("filesystem", jail.path(), &denied);
    assert!(
        output.status.success(),
        "helper failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn network_denied_policy_blocks_ip_sockets() {
    if !backend_enforces_filesystem() {
        eprintln!("skipping: no confinement on this host");
        return;
    }

    let jail = tempfile::tempdir().expect("tempdir");
    let unused = jail.path().join("unused");
    std::fs::write(&unused, b"x").expect("write");

    let output = run_helper("network_denied", jail.path(), &unused);
    assert!(
        output.status.success(),
        "helper failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn outbound_policy_allows_sockets_but_refuses_listeners() {
    if !backend_enforces_filesystem() {
        eprintln!("skipping: no confinement on this host");
        return;
    }

    let jail = tempfile::tempdir().expect("tempdir");
    let output = run_helper("network_outbound", jail.path(), &jail.path().join("unused"));
    assert!(
        output.status.success(),
        "helper failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// The Audible guest binds a loopback callback server during interactive login,
/// so the plugin jail has to permit that one bind without opening up the rest.
#[test]
fn outbound_listen_policy_allows_an_oauth_callback_listener() {
    if !backend_enforces_filesystem() {
        eprintln!("skipping: no confinement on this host");
        return;
    }

    let jail = tempfile::tempdir().expect("tempdir");
    let output = run_helper(
        "network_outbound_listen",
        jail.path(),
        &jail.path().join("unused"),
    );
    assert!(
        output.status.success(),
        "helper failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn required_enforcement_fails_when_backend_is_missing() {
    // On a host with no backend, `Required` must surface an error rather than
    // running unconfined. Where a backend exists this asserts the happy path.
    let jail = tempfile::tempdir().expect("tempdir");
    let policy = Policy::new("probe")
        // Without the system set the only writable entry is the declared one,
        // which keeps this about path resolution rather than about `/dev/null`.
        .system_paths(false)
        .write(jail.path())
        .enforcement(Enforcement::Required);

    if backend_enforces_filesystem() {
        // Confinement is irreversible, so only check that the policy resolves.
        // Compared by physical location rather than by spelling: resolution is
        // what makes the rule match on macOS, where the temp dir is reached
        // through a symlink, and Windows entries drop a `\\?\` prefix.
        let resolved = policy.resolved_writes();
        assert_eq!(resolved.len(), 1, "expected one entry, got {resolved:?}");
        assert_eq!(
            std::fs::canonicalize(&resolved[0]).expect("canonicalize entry"),
            std::fs::canonicalize(jail.path()).expect("canonicalize jail"),
        );
    } else {
        let err = policy
            .confine_current_process()
            .expect_err("Required must fail without a backend");
        assert!(err.to_string().contains("not enforced"), "got: {err}");
    }
}

/// Pick a port below the kernel's ephemeral range for Landlock fixed-port probes.
#[cfg(target_os = "linux")]
fn linux_fixed_port_outside_ephemeral() -> u16 {
    let range = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
        .unwrap_or_else(|_| "32768\t60999".into());
    let min = range
        .split_whitespace()
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(32768);
    min.saturating_sub(1).max(1024)
}
