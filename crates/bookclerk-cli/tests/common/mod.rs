//! Shared harness for `bookclerk` binary tests: a files directory with the
//! platform sqlite guest staged, a bootstrap-only `config.toml`, and a runner
//! that scrubs ambient Bookclerk environment.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn bookclerk_bin() -> &'static str {
    env!("CARGO_BIN_EXE_bookclerk")
}

pub fn sqlite_guest_name() -> String {
    format!(
        "bookclerk-plugin-database-sqlite{}",
        std::env::consts::EXE_SUFFIX
    )
}

pub fn sqlite_plugin_bin() -> PathBuf {
    Path::new(bookclerk_bin())
        .parent()
        .expect("target dir")
        .join(sqlite_guest_name())
}

pub fn stage_sqlite_plugin(files: &Path) {
    let key =
        bookclerk_plugin_catalog::PluginKey::platform("bookclerk-plugin-database-sqlite", "sqlite")
            .expect("platform key");
    let plugin = files.join("plugins").join(key.fs_id());
    fs::create_dir_all(&plugin).unwrap();
    let bin = sqlite_plugin_bin();
    assert!(
        bin.is_file(),
        "build the sqlite guest before this test: {}",
        bin.display()
    );
    let guest = sqlite_guest_name();
    let dest = plugin.join(&guest);
    if fs::hard_link(&bin, &dest).is_err() {
        fs::copy(&bin, &dest).unwrap();
    }
    let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../bookclerk-plugins/platform/database-sqlite/plugin.toml");
    let staged = plugin.join("plugin.toml");
    let template = fs::read_to_string(&manifest_path).unwrap();
    let command = format!("./{guest}");
    let text = template.replacen(
        "command = \"./bookclerk-plugin-database-sqlite\"",
        &format!("command = \"{command}\""),
        1,
    );
    assert!(
        text.contains(&format!("command = \"{command}\"")),
        "staged manifest must name the guest executable"
    );
    fs::write(&staged, text).unwrap();
    let text = fs::read_to_string(&staged).unwrap();
    let manifest = bookclerk_plugin_manifest::PluginManifest::parse(&text).expect("manifest");
    assert_eq!(manifest.id, "sqlite");
    bookclerk_plugin_catalog::stamp_platform_receipt(
        &plugin,
        files,
        "bookclerk-plugin-database-sqlite",
        &manifest,
        "0.1.0",
    )
    .expect("stamp platform sqlite");
}

pub fn write_config(files: &Path) {
    fs::write(
        files.join("config.toml"),
        "\
[database]
plugin = \"sqlite\"

[plugins]
isolation = \"required\"

[daemon]
listen = \"127.0.0.1:9\"

[daemon.auth]
enabled = true
",
    )
    .unwrap();
}

/// A `bookclerk` command isolated to `files`, with no inherited
/// configuration from the environment.
pub fn bookclerk_command(files: &Path) -> Command {
    let mut cmd = Command::new(bookclerk_bin());
    cmd.env("BOOKCLERK_FILES_DIR", files)
        .env_remove("BOOKCLERK_PLUGIN_DIRS")
        .env_remove("BOOKCLERK_AUTH_PASSWORD")
        .env_remove("BOOKCLERK_CONFIG")
        .env_remove("BOOKCLERK_DATABASE_PLUGIN")
        .env_remove("BOOKCLERK_DATABASE_SQLITE_PATH")
        .env_remove("BOOKCLERK_EVENTS_RETENTION_DAYS")
        .env_remove("BOOKCLERK_EVENTS_DEAD_LETTER_RETENTION_DAYS")
        .env_remove("BOOKCLERK_EVENTS_CONCURRENCY")
        .env_remove("BOOKCLERK_OPERATOR_TOKEN");
    cmd
}

pub fn run_bookclerk(files: &Path, args: &[&str], extra: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = bookclerk_command(files);
    cmd.args(args);
    for (key, value) in extra {
        cmd.env(key, value);
    }
    cmd.output().expect("spawn bookclerk")
}

pub fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
