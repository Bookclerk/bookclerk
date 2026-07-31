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
const SECRET: &str = "BOOKCLERK_SANDBOX_TEST_SECRET";
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
fn run_helper(role: &str, allowed: &Path, secret: &Path) -> std::process::Output {
    let exe = std::env::current_exe().expect("current_exe");
    Command::new(exe)
        .arg("--nocapture")
        // Run only the helper entry point, not the whole suite.
        .arg("helper_entry_point")
        .env(ROLE, role)
        .env(ALLOWED, allowed)
        .env(SECRET, secret)
        .output()
        .expect("spawn helper")
}

/// The child side. Dispatches on `ROLE`; a normal test run has it unset and
/// this returns immediately.
#[test]
fn helper_entry_point() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    let allowed = std::path::PathBuf::from(std::env::var(ALLOWED).expect("ALLOWED"));
    let secret = std::path::PathBuf::from(std::env::var(SECRET).expect("SECRET"));

    let outcome = match role.as_str() {
        "filesystem" => child_filesystem(&allowed, &secret),
        "network_denied" => child_network_denied(&allowed),
        "media_worker_shape" => child_media_worker_shape(&allowed, &secret),
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
fn child_filesystem(allowed: &Path, secret: &Path) -> Result<(), String> {
    // Prove the secret is readable *before* confinement, so a failure after it
    // cannot be blamed on a bad path or missing fixture.
    std::fs::read_to_string(secret)
        .map_err(|err| format!("secret unreadable before confinement: {err}"))?;

    let report = Policy::new("test-filesystem")
        .write(allowed)
        .enforcement(Enforcement::Required)
        .confine_current_process()
        .map_err(|err| format!("confinement failed: {err}"))?;

    if !report.is_confined() {
        return Err(format!("report says unconfined: {}", report.summary()));
    }

    if std::fs::read_to_string(secret).is_ok() {
        return Err(format!(
            "read the secret at {} from inside the jail",
            secret.display()
        ));
    }

    // The allowlist must still work, or the jail is useless rather than secure.
    let scratch = allowed.join("written-inside-jail.txt");
    std::fs::write(&scratch, b"ok").map_err(|err| format!("write inside allowlist: {err}"))?;
    std::fs::read_to_string(&scratch).map_err(|err| format!("read inside allowlist: {err}"))?;

    // Creating a *sibling* of the allowed dir must fail; otherwise the rule was
    // applied to the parent rather than the directory itself.
    let escape = allowed.join("..").join("escaped.txt");
    if std::fs::write(&escape, b"nope").is_ok() {
        return Err("wrote outside the allowlist via a parent traversal".to_string());
    }

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

/// Mirrors the policy `bookclerk-media-worker` builds: read the job's input
/// file, write the job's output directory, no network.
///
/// The worker derives its allowlist from the job, so this is the shape that
/// actually has to hold in production — a media job must not be able to reach
/// `master.key` or `library.db` even though they sit under the same files dir.
fn child_media_worker_shape(job_dir: &Path, files_dir: &Path) -> Result<(), String> {
    let input = job_dir.join("book.m4b");
    let output_dir = job_dir.join("out");
    std::fs::create_dir_all(&output_dir).map_err(|err| format!("create output dir: {err}"))?;

    let master_key = files_dir.join("master.key");
    let library_db = files_dir.join("library.db");

    Policy::new("media-worker:encode_mp3")
        .read(&input)
        .write(&output_dir)
        .net(NetPolicy::Deny)
        .allow_exec(false)
        .enforcement(Enforcement::Required)
        .confine_current_process()
        .map_err(|err| format!("confinement failed: {err}"))?;

    for secret in [&master_key, &library_db] {
        if std::fs::read(secret).is_ok() {
            return Err(format!("media job read {}", secret.display()));
        }
    }

    // The job's own paths must still work.
    std::fs::read(&input).map_err(|err| format!("declared input unreadable: {err}"))?;
    std::fs::write(output_dir.join("encoded.mp3"), b"out")
        .map_err(|err| format!("declared output dir unwritable: {err}"))?;

    // Writing into the files dir must fail even though a sibling is writable.
    if std::fs::write(files_dir.join("planted"), b"x").is_ok() {
        return Err("media job wrote into the files dir".to_string());
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
    std::fs::write(job_dir.path().join("book.m4b"), b"fake audio").expect("write input");
    std::fs::write(files_dir.path().join("master.key"), b"sealed-dek").expect("write key");
    std::fs::write(files_dir.path().join("library.db"), b"sqlite").expect("write db");

    let output = run_helper("media_worker_shape", job_dir.path(), files_dir.path());
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
    let secret = vault.path().join("master.key");
    std::fs::write(&secret, b"pretend-this-is-a-data-encryption-key").expect("write secret");

    let output = run_helper("filesystem", jail.path(), &secret);
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
    let secret = jail.path().join("unused");
    std::fs::write(&secret, b"x").expect("write");

    let output = run_helper("network_denied", jail.path(), &secret);
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
