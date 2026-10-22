//! Author-tools conformance against shared fixtures.

// Test code: unwrap/expect and plain casts are fine here.
#![allow(clippy::expect_used)]

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

/// Copies the `valid-compat-date-future` fixture into a temp dir with its
/// `compatibility_date` set to the day after the workerd pin, so the boundary
/// tracks every pin bump.
///
/// # Returns
///
/// The temp dir holding the rewritten fixture; dropping it removes the copy.
fn future_date_fixture() -> tempfile::TempDir {
    let src =
        repo_root().join("crates/bookclerk-plugin-abi/fixtures/tools/valid-compat-date-future");
    let newer = bookclerk_plugin_manifest::calendar_day_after(
        bookclerk_plugin_manifest::WORKERD_PIN_COMPAT_DATE,
    )
    .expect("day after pin");
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = std::fs::read_to_string(src.join("plugin.toml")).expect("read fixture");
    let rewritten: String = manifest
        .lines()
        .map(|line| {
            if line.starts_with("compatibility_date = ") {
                format!("compatibility_date = \"{newer}\"\n")
            } else {
                format!("{line}\n")
            }
        })
        .collect();
    assert!(
        rewritten.contains(&newer),
        "fixture has no compatibility_date"
    );
    std::fs::write(dir.path().join("plugin.toml"), rewritten).expect("write fixture");
    std::fs::create_dir(dir.path().join("modules")).expect("modules dir");
    std::fs::copy(
        src.join("modules/index.js"),
        dir.path().join("modules/index.js"),
    )
    .expect("copy module");
    dir
}

#[test]
fn check_warns_when_compatibility_date_is_newer_than_pin() {
    let dir = future_date_fixture();
    let out = bookclerk_plugin()
        .args(["check", dir.path().to_str().expect("utf8 path")])
        .output()
        .expect("run check");
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
fn check_rejects_module_path_when_name_matches_a_different_file() {
    assert_check_fails("invalid-module-path", "missing.js");
    assert_check_fails("invalid-module-path", "not in the workerd load set");
}

#[test]
fn check_accepts_explicit_path_over_py_name() {
    let path_wins = check_fixture("valid-module-path-wins");
    assert!(
        path_wins.status.success(),
        "explicit path must win over a .py name: {}",
        String::from_utf8_lossy(&path_wins.stderr)
    );
    let helper = check_fixture("valid-js-with-python-helper");
    assert!(
        helper.status.success(),
        "JS main with a Python helper must lint as JS: {}",
        String::from_utf8_lossy(&helper.stderr)
    );
}

#[test]
fn check_accepts_module_name_when_path_is_omitted() {
    let out = check_fixture("valid-module-name-only");
    assert!(
        out.status.success(),
        "omitted path should use name: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn check_rejects_kv_oauth_binding() {
    assert_check_fails("invalid-kv-oauth", "collides");
    assert_check_fails("invalid-kv-secret", "collides");
    assert_check_fails("invalid-kv-work-fs", "collides");
    assert_check_fails("invalid-kv-oauth-name", "collides");
    assert_check_fails("invalid-producer-database", "collides");
    let shared = check_fixture("valid-producer-shares-oauth-name");
    assert!(
        shared.status.success(),
        "a producer may reuse the oauth binding name ({})",
        u8::from(shared.status.success())
    );
}

#[test]
fn check_rejects_undeclared_python_file() {
    assert_check_fails("invalid-undeclared-python", "undeclared Python file");
}

#[test]
fn check_rejects_typescript_main_as_not_implemented() {
    assert_check_fails("invalid-module-ts", "not implemented yet");
}

#[test]
fn check_accepts_kv_declaration() {
    let out = check_fixture("valid-kv");
    assert!(
        out.status.success(),
        "kv declaration must be legal: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn check_rejects_bad_kv_namespace() {
    assert_check_fails("invalid-kv-namespace", "namespace");
}

#[test]
fn check_rejects_plugin_producing_host_event() {
    assert_check_fails("invalid-producer-host-event", "emitted by the host");
}

#[test]
fn check_accepts_queues_declaration() {
    let out = check_fixture("valid-queues");
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
