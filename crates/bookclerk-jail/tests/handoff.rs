//! Proof that the program `bookclerk-jail` hands off to is actually confined.
//!
//! The launcher's whole reason to exist is that the restrictions survive `exec`,
//! so the assertions here run in the exec'd program rather than in the launcher.
//! A test that only checked the launcher's own view would pass against a jail
//! that stops at the handoff, which is the failure mode that matters.

#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Output};

use bookclerk_sandbox::{Enforcement, NetPolicy, Spec};

const JAIL: &str = env!("CARGO_BIN_EXE_bookclerk-jail");

/// Whether this host can enforce a filesystem allowlist.
///
/// Mirrors `bookclerk-sandbox`'s enforcement tests: a CI runner that is expected
/// to confine sets `BOOKCLERK_SANDBOX_REQUIRE_ENFORCEMENT`, which turns a skip
/// into a failure so a green run always means something.
fn confinement_available() -> bool {
    let caps = bookclerk_sandbox::capabilities();
    let demanded = std::env::var("BOOKCLERK_SANDBOX_REQUIRE_ENFORCEMENT")
        .is_ok_and(|value| !value.trim().is_empty());
    assert!(
        caps.filesystem || !demanded,
        "enforcement demanded but unavailable: {} [{}]",
        caps.detail,
        caps.backend
    );
    caps.filesystem
}

/// Write an executable shell script.
fn script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("write script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

fn run_script(script: &Path, args: &[&Path], envs: &[(&str, &Path)]) -> Output {
    run_script_with_extra_env(script, args, envs, &[])
}

fn run_script_with_extra_env(
    script: &Path,
    args: &[&Path],
    path_envs: &[(&str, &Path)],
    extra_envs: &[(&str, String)],
) -> Output {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg(script).args(args);
    for (key, value) in path_envs {
        cmd.env(key, value);
    }
    for (key, value) in extra_envs {
        cmd.env(key, value);
    }
    cmd.output().expect("run script")
}

fn run_jailed(spec: &Spec, program: &Path, envs: &[(&str, &Path)]) -> Output {
    let mut cmd = Command::new(JAIL);
    cmd.arg("/bin/sh").arg(program).env(
        bookclerk_sandbox::SPEC_ENV,
        serde_json::to_string(spec).expect("encode spec"),
    );
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.output().expect("run bookclerk-jail")
}

/// A guest killed by the sandbox says nothing at all, so the status has to be
/// part of the message or the failure looks like an empty one.
fn assert_ok(output: &Output) {
    use std::os::unix::process::ExitStatusExt;

    assert!(
        output.status.success(),
        "jailed program failed: {} (signal {:?})\nstdout: {}\nstderr: {}",
        output.status,
        output.status.signal(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// The exec'd program must inherit the allowlist in both directions.
#[test]
fn the_exec_target_inherits_the_jail() {
    if !confinement_available() {
        eprintln!("skipping: no filesystem confinement on this host");
        return;
    }

    let jail = tempfile::tempdir().expect("tempdir");
    let vault = tempfile::tempdir().expect("tempdir");
    let secret = vault.path().join("master.key");
    std::fs::write(&secret, b"sealed-dek").expect("write secret");

    let guest = jail.path().join("guest.sh");
    script(
        &guest,
        r#"
if cat "$SECRET" >/dev/null 2>&1; then
  echo "read a file outside the allowlist" >&2
  exit 1
fi
if ! echo ok > "$ALLOWED/written-by-guest"; then
  echo "could not write inside the allowlist" >&2
  exit 1
fi
if [ -n "$BOOKCLERK_JAIL_SPEC" ]; then
  echo "guest can see its own jail spec" >&2
  exit 1
fi
exit 0
"#,
    );

    let spec = Spec {
        writes: vec![jail.path().to_path_buf()],
        // The launcher has to exec to hand over, so this is always on for a
        // jailed guest; see the module docs on why it costs little.
        allow_exec: true,
        enforcement: Enforcement::Required,
        ..Spec::new("test:handoff")
    };

    let output = run_jailed(
        &spec,
        &guest,
        &[("SECRET", secret.as_path()), ("ALLOWED", jail.path())],
    );
    assert_ok(&output);
    assert!(
        jail.path().join("written-by-guest").exists(),
        "guest should have written inside the jail"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("bookclerk-jail:"),
        "launcher should report what it applied: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// An open descriptor is past the allowlist for good, so the jail has to be the
/// thing that takes it away.
///
/// Reading `<&3` names no path, which means no policy can answer it. The only
/// question is whether the descriptor is still there after the handoff, and the
/// unjailed half of this test is what makes the jailed half mean anything: it
/// shows the probe really does leak one.
#[test]
fn an_inherited_descriptor_does_not_survive_the_handoff() {
    if !confinement_available() {
        eprintln!("skipping: no filesystem confinement on this host");
        return;
    }

    let jail = tempfile::tempdir().expect("tempdir");
    let vault = tempfile::tempdir().expect("tempdir");
    let secret = vault.path().join("master.key");
    // Newline-terminated so `read` reports success rather than EOF.
    std::fs::write(&secret, b"sealed-dek\n").expect("write secret");

    let guest = jail.path().join("guest.sh");
    script(
        &guest,
        r#"
if read -r line <&3 2>/dev/null; then
  echo "inherited: $line"
else
  echo "no descriptor"
fi
"#,
    );

    // `exec 3<` opens without CLOEXEC, which is what a host that leaked a
    // descriptor across the spawn would look like.
    let opener = jail.path().join("leak-fd-3.sh");
    script(
        &opener,
        r#"
exec 3< "$SECRET"
exec "$@"
"#,
    );

    let unjailed = run_script(&opener, &[guest.as_path()], &[("SECRET", secret.as_path())]);
    assert_eq!(
        String::from_utf8_lossy(&unjailed.stdout).trim(),
        "inherited: sealed-dek",
        "the probe must leak a descriptor, or the jailed half proves nothing"
    );

    let spec = Spec {
        writes: vec![jail.path().to_path_buf()],
        allow_exec: true,
        enforcement: Enforcement::Required,
        ..Spec::new("test:handoff-fds")
    };
    let jailed = run_script_with_extra_env(
        &opener,
        &[Path::new(JAIL), Path::new("/bin/sh"), guest.as_path()],
        &[("SECRET", secret.as_path())],
        &[(
            bookclerk_sandbox::SPEC_ENV,
            serde_json::to_string(&spec).expect("encode spec"),
        )],
    );
    assert_ok(&jailed);
    assert_eq!(
        String::from_utf8_lossy(&jailed.stdout).trim(),
        "no descriptor",
        "the jail handed an inherited descriptor to the guest"
    );
}

/// A guest that reaches for the network under `Deny` must be refused, so the
/// policy travelling as JSON keeps its meaning.
#[test]
fn a_denied_network_survives_the_handoff() {
    if !confinement_available() {
        eprintln!("skipping: no filesystem confinement on this host");
        return;
    }

    let jail = tempfile::tempdir().expect("tempdir");
    let guest = jail.path().join("guest.sh");
    // `/dev/tcp` is a bash builtin, so use the interpreter that has it. Failing
    // to open the socket is the pass condition.
    script(
        &guest,
        r#"
if command -v curl >/dev/null 2>&1; then
  if curl -s -m 2 http://127.0.0.1:9 >/dev/null 2>&1; then
    echo "curl reached the network under NetPolicy::Deny" >&2
    exit 1
  fi
fi
exit 0
"#,
    );

    let spec = Spec {
        writes: vec![jail.path().to_path_buf()],
        net: NetPolicy::Deny,
        allow_exec: true,
        enforcement: Enforcement::Required,
        ..Spec::new("test:handoff-net")
    };
    assert_ok(&run_jailed(&spec, &guest, &[]));
}

/// Nothing runs when the jail cannot be built. The guest binary must not be
/// reached, or a broken policy would silently become no policy.
#[test]
fn a_guest_does_not_run_when_the_jail_cannot_be_applied() {
    let jail = tempfile::tempdir().expect("tempdir");
    let guest = jail.path().join("guest.sh");
    let marker = jail.path().join("guest-ran");
    script(&guest, &format!("touch {}", marker.display()));

    let spec = Spec {
        // A path that is not there would otherwise be dropped from the
        // allowlist, quietly narrowing the jail.
        writes: vec![jail.path().join("does-not-exist")],
        allow_exec: true,
        enforcement: Enforcement::Required,
        ..Spec::new("test:handoff-broken")
    };

    let output = run_jailed(&spec, &guest, &[]);
    assert!(!output.status.success(), "launcher should have refused");
    assert!(!marker.exists(), "guest ran despite a jail that failed");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("do not exist"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Without a spec there is no jail, so there must be no guest either.
#[test]
fn no_spec_means_no_guest() {
    let jail = tempfile::tempdir().expect("tempdir");
    let guest = jail.path().join("guest.sh");
    let marker = jail.path().join("guest-ran");
    script(&guest, &format!("touch {}", marker.display()));

    let output = Command::new(JAIL)
        .arg(&guest)
        .env_remove(bookclerk_sandbox::SPEC_ENV)
        .output()
        .expect("run bookclerk-jail");

    assert!(!output.status.success());
    assert!(!marker.exists(), "guest ran unconfined");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("refusing to run unconfined"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
