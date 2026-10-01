//! Author-tools conformance against shared fixtures.

use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn bookclerk_plugin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_bookclerk-plugin"));
    cmd.current_dir(repo_root());
    cmd
}

fn check_fixture(name: &str) -> std::process::Output {
    let dir = repo_root()
        .join("crates/bookclerk-plugin-abi/fixtures/tools")
        .join(name);
    bookclerk_plugin()
        .args(["check", dir.to_str().expect("utf8 path")])
        .output()
        .expect("run check")
}

fn assert_check_fails(name: &str, needle: &str) {
    let out = check_fixture(name);
    assert!(!out.status.success(), "{name} should fail");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains(needle),
        "{name} stderr missing {needle:?}: {err}"
    );
}

#[test]
fn check_valid_workerd() {
    let dir = repo_root().join("crates/bookclerk-plugin-abi/fixtures/tools/valid-workerd");
    let out = bookclerk_plugin()
        .args(["check", dir.to_str().unwrap()])
        .output()
        .expect("run check");
    assert!(out.status.success(), "{:?}", out);
}

#[test]
fn check_accepts_leading_separator_from_cargo_alias() {
    // `cargo plugin -- check <dir>` forwards `--` before the subcommand.
    let dir = repo_root().join("crates/bookclerk-plugin-abi/fixtures/tools/valid-workerd");
    let out = bookclerk_plugin()
        .args(["--", "check", dir.to_str().unwrap()])
        .output()
        .expect("run check");
    assert!(out.status.success(), "{:?}", out);
}

#[test]
fn check_rejects_outbound_without_domains() {
    let dir =
        repo_root().join("crates/bookclerk-plugin-abi/fixtures/tools/invalid-outbound-no-domains");
    let out = bookclerk_plugin()
        .args(["check", dir.to_str().unwrap()])
        .output()
        .expect("run check");
    assert!(!out.status.success());
}

#[test]
fn check_valid_logo_url() {
    let dir = repo_root().join("crates/bookclerk-plugin-abi/fixtures/tools/valid-logo-url");
    let out = bookclerk_plugin()
        .args(["check", dir.to_str().unwrap()])
        .output()
        .expect("run check");
    assert!(out.status.success(), "{:?}", out);
}

#[test]
fn check_valid_logo_path() {
    let dir = repo_root().join("crates/bookclerk-plugin-abi/fixtures/tools/valid-logo-path");
    let out = bookclerk_plugin()
        .args(["check", dir.to_str().unwrap()])
        .output()
        .expect("run check");
    assert!(out.status.success(), "{:?}", out);
}

#[test]
fn check_rejects_logo_javascript() {
    let dir =
        repo_root().join("crates/bookclerk-plugin-abi/fixtures/tools/invalid-logo-javascript");
    let out = bookclerk_plugin()
        .args(["check", dir.to_str().unwrap()])
        .output()
        .expect("run check");
    assert!(!out.status.success());
}

#[test]
fn check_rejects_logo_vbscript() {
    let dir = repo_root().join("crates/bookclerk-plugin-abi/fixtures/tools/invalid-logo-vbscript");
    let out = bookclerk_plugin()
        .args(["check", dir.to_str().unwrap()])
        .output()
        .expect("run check");
    assert!(!out.status.success());
}

#[test]
fn check_rejects_logo_parent() {
    let dir = repo_root().join("crates/bookclerk-plugin-abi/fixtures/tools/invalid-logo-parent");
    let out = bookclerk_plugin()
        .args(["check", dir.to_str().unwrap()])
        .output()
        .expect("run check");
    assert!(!out.status.success());
}

#[test]
fn check_warns_when_compatibility_date_is_newer_than_pin() {
    let out = check_fixture("valid-compat-date-future");
    assert!(
        out.status.success(),
        "newer date should load: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("Falling back"),
        "stderr missing fallback warning: {err}"
    );
}

#[test]
fn check_rejects_non_calendar_compatibility_date() {
    assert_check_fails("invalid-compat-date-shape", "YYYY-MM-DD");
}

#[test]
fn check_rejects_unknown_compatibility_flag() {
    assert_check_fails("invalid-compat-flag", "not allowed");
}

#[test]
fn check_rejects_experimental_compatibility_flag() {
    assert_check_fails("invalid-compat-experimental", "host-only");
}

#[test]
fn check_rejects_python_without_flag_pair() {
    assert_check_fails("invalid-python-flags-missing", "must include");
}

#[test]
fn check_rejects_python_flags_without_python_module() {
    assert_check_fails("invalid-flags-without-python", "Python module");
}

#[test]
fn check_rejects_module_type_mismatch() {
    assert_check_fails("invalid-module-type", "does not match");
}

#[test]
fn check_rejects_typescript_main_as_not_implemented() {
    assert_check_fails("invalid-module-ts", "not implemented yet");
}

#[test]
fn check_accepts_kv_declaration() {
    let out = check_fixture("not-implemented-kv");
    assert!(
        out.status.success(),
        "kv declaration must stay legal: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn check_accepts_queues_declaration() {
    let out = check_fixture("not-implemented-queues");
    assert!(
        out.status.success(),
        "queues declaration must stay legal: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn check_rejects_native_with_domains() {
    let dir =
        repo_root().join("crates/bookclerk-plugin-abi/fixtures/tools/invalid-native-with-domains");
    let out = bookclerk_plugin()
        .args(["check", dir.to_str().unwrap()])
        .output()
        .expect("run check");
    assert!(!out.status.success());
}

#[test]
fn fmt_check_gold_native() {
    let file =
        repo_root().join("crates/bookclerk-plugin-abi/fixtures/tools/valid-native/plugin.fmt.toml");
    let out = bookclerk_plugin()
        .args(["fmt", "--check", file.to_str().unwrap()])
        .output()
        .expect("run fmt");
    assert!(
        out.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
