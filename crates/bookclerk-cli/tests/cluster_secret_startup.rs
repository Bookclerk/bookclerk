//! Command startup must not mint `master.key` before a command decides to open
//! the library. Enrolled-database commands go through `open_library`, which
//! aligns the cluster secret root before any seal. A fresh sqlite guest in
//! this environment fails host schema apply with a pre-existing
//! `bookclerk_sql_catalog` unique violation, so the missing-key and wrong-key
//! cases are covered by the library tests and by the daemon settings tests
//! rather than by another subprocess schema apply.

use std::fs;
use std::process::Command;

use bookclerk_library::master_key_path;

fn bookclerk_bin() -> &'static str {
    env!("CARGO_BIN_EXE_bookclerk")
}

#[test]
fn version_and_master_key_commands_do_not_mint() {
    let files = tempfile::tempdir().unwrap();
    fs::create_dir_all(files.path()).unwrap();
    let version = Command::new(bookclerk_bin())
        .env("BOOKCLERK_FILES_DIR", files.path())
        .env_remove("BOOKCLERK_AUTH_PASSWORD")
        .env_remove("BOOKCLERK_PLUGIN_DIRS")
        .args(["version"])
        .output()
        .unwrap();
    assert!(
        version.status.success(),
        "{}",
        String::from_utf8_lossy(&version.stderr)
    );
    assert!(!master_key_path(files.path()).exists());

    let status = Command::new(bookclerk_bin())
        .env("BOOKCLERK_FILES_DIR", files.path())
        .env_remove("BOOKCLERK_AUTH_PASSWORD")
        .args(["config", "master-key", "status"])
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(!master_key_path(files.path()).exists());
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(stdout.contains("missing"), "{stdout}");

    let wrap = Command::new(bookclerk_bin())
        .env("BOOKCLERK_FILES_DIR", files.path())
        .env("BOOKCLERK_AUTH_PASSWORD", "test-passphrase")
        .args(["config", "master-key", "wrap"])
        .output()
        .unwrap();
    assert!(!wrap.status.success(), "wrap must not mint a missing key");
    assert!(!master_key_path(files.path()).exists());
}
