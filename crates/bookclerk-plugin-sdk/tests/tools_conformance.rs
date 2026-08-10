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
