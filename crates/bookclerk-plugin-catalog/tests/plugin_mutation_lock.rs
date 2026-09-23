//! Subprocess tests for the host-local plugin mutation lock.
//!
//! File locks are process-scoped on some platforms, so exclusivity is proven
//! with child processes rather than threads. Distinct `$FILES_DIR` values must
//! not block each other; the same `$FILES_DIR` serializes install/remove.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use bookclerk_plugin_catalog::{
    host_bookclerk_target, ArtifactTarget, BookclerkPackageManifest, InstallLedger, InstallOptions,
    Installer, PackageCoordinate, PluginKind, PluginMutationLock, RegistrySource, TrustPolicy,
};
use flate2::write::GzEncoder;
use flate2::Compression;
use tar::Builder;

/// Env var selecting child process mode (`install`, `remove`, `hold`).
const CHILD_ENV: &str = "BOOKCLERK_PLUGIN_MUTATION_CHILD";
/// Host `$FILES_DIR` for the child operation.
const FILES_DIR_ENV: &str = "BOOKCLERK_PLUGIN_MUTATION_FILES_DIR";
/// Local archive path for a child install.
const ARCHIVE_ENV: &str = "BOOKCLERK_PLUGIN_MUTATION_ARCHIVE";
/// Runtime alias for install or remove.
const ALIAS_ENV: &str = "BOOKCLERK_PLUGIN_MUTATION_ALIAS";
/// Milliseconds a `hold` child keeps the mutation lock.
const HOLD_MS_ENV: &str = "BOOKCLERK_PLUGIN_MUTATION_HOLD_MS";
/// When `1`, child install uses `--replace`.
const REPLACE_ENV: &str = "BOOKCLERK_PLUGIN_MUTATION_REPLACE";

/// Runs child mode when env is set; otherwise executes the lock test suite.
fn main() {
    if let Ok(op) = std::env::var(CHILD_ENV) {
        match run_child(&op) {
            Ok(()) => std::process::exit(0),
            Err(err) => {
                eprintln!("{err}");
                std::process::exit(1);
            }
        }
    }
    let mut failed = 0;
    for (name, test) in [
        (
            "same_alias_one_winner",
            same_alias_one_winner as fn() -> Result<(), String>,
        ),
        (
            "distinct_aliases_keep_both_ledger_rows",
            distinct_aliases_keep_both_ledger_rows,
        ),
        ("update_and_remove_serialize", update_and_remove_serialize),
        (
            "distinct_files_dirs_do_not_block",
            distinct_files_dirs_do_not_block,
        ),
        (
            "same_files_dir_blocks_while_held",
            same_files_dir_blocks_while_held,
        ),
    ] {
        eprint!("test {name} ... ");
        match test() {
            Ok(()) => eprintln!("ok"),
            Err(err) => {
                eprintln!("FAILED\n{err}");
                failed += 1;
            }
        }
    }
    if failed > 0 {
        std::process::exit(1);
    }
}

/// Dispatches a child mutation operation.
fn run_child(op: &str) -> Result<(), String> {
    match op {
        "install" => child_install(),
        "remove" => child_remove(),
        "hold" => child_hold(),
        other => Err(format!("unknown child op {other}")),
    }
}

/// Installs one archive into the child files dir (acquires the host lock).
fn child_install() -> Result<(), String> {
    let files_dir = env_path(FILES_DIR_ENV)?;
    let archive = env_path(ARCHIVE_ENV)?;
    let alias = std::env::var(ALIAS_ENV).map_err(|err| err.to_string())?;
    let replace = std::env::var(REPLACE_ENV).ok().as_deref() == Some("1");
    let plugins = files_dir.join("plugins");
    let digest = bookclerk_plugin_catalog::sha256_file(&archive).map_err(|err| err.to_string())?;
    let target = host_bookclerk_target();
    let manifest = test_manifest(
        &alias,
        digest,
        format!("file://{}", archive.display()),
        target,
    );
    let opts = InstallOptions {
        plugins_root: plugins,
        replace,
        trust: TrustPolicy::allow_unverified_publisher(),
        ..Default::default()
    };
    let coord = PackageCoordinate {
        source: RegistrySource::LocalArchive,
        name: archive.display().to_string(),
        version: "1.0.0".into(),
    };
    let out = Installer::install_from_manifest(&manifest, &coord, &opts)
        .map_err(|err| err.to_string())?;
    Installer::commit(&out).map_err(|err| err.to_string())?;
    Ok(())
}

/// Removes `ALIAS_ENV` from the child files dir.
fn child_remove() -> Result<(), String> {
    let files_dir = env_path(FILES_DIR_ENV)?;
    let alias = std::env::var(ALIAS_ENV).map_err(|err| err.to_string())?;
    Installer::remove(&files_dir.join("plugins"), &alias, false).map_err(|err| err.to_string())
}

/// Holds the host mutation lock for `HOLD_MS_ENV` milliseconds.
fn child_hold() -> Result<(), String> {
    let files_dir = env_path(FILES_DIR_ENV)?;
    let ms: u64 = std::env::var(HOLD_MS_ENV)
        .unwrap_or_else(|_| "2000".into())
        .parse()
        .map_err(|err: std::num::ParseIntError| err.to_string())?;
    let _lock = PluginMutationLock::acquire(&files_dir).map_err(|err| err.to_string())?;
    std::thread::sleep(Duration::from_millis(ms));
    Ok(())
}

/// Reads a required path-valued environment variable.
fn env_path(name: &str) -> Result<PathBuf, String> {
    Ok(PathBuf::from(
        std::env::var(name).map_err(|err| format!("{name}: {err}"))?,
    ))
}

/// Two processes racing the same alias: exactly one install wins.
fn same_alias_one_winner() -> Result<(), String> {
    let tmp = tempfile::tempdir().map_err(|err| err.to_string())?;
    let files = tmp.path();
    let (archive_a, _) = make_named_archive(files, "a.tar.gz", "foo");
    let (archive_b, _) = make_named_archive(files, "b.tar.gz", "foo");
    let child_a = spawn_install(files, &archive_a, "foo", false)?;
    let child_b = spawn_install(files, &archive_b, "foo", false)?;
    let status_a = wait_child(child_a, "same-alias-a")?;
    let status_b = wait_child(child_b, "same-alias-b")?;
    let wins = u8::from(status_a.success()) + u8::from(status_b.success());
    if wins != 1 {
        return Err(format!(
            "expected exactly one install to succeed, got a={} b={}",
            status_a.success(),
            status_b.success()
        ));
    }
    let ledger = InstallLedger::load(files).map_err(|err| err.to_string())?;
    let foo_rows: Vec<_> = ledger
        .artifacts
        .iter()
        .filter(|row| row.manifest_id.eq_ignore_ascii_case("foo"))
        .collect();
    if foo_rows.len() != 1 {
        return Err(format!(
            "expected one ledger row for alias foo, got {}",
            foo_rows.len()
        ));
    }
    let plugins = files.join("plugins");
    let trees: Vec<_> = installed_plugin_dirs(&plugins)?;
    if trees.len() != 1 {
        return Err(format!("expected one plugin tree, got {trees:?}"));
    }
    Ok(())
}

/// Concurrent distinct aliases under one files dir keep both ledger rows.
fn distinct_aliases_keep_both_ledger_rows() -> Result<(), String> {
    let tmp = tempfile::tempdir().map_err(|err| err.to_string())?;
    let files = tmp.path();
    let (archive_a, _) = make_named_archive(files, "a.tar.gz", "foo");
    let (archive_b, _) = make_named_archive(files, "b.tar.gz", "bar");
    let child_a = spawn_install(files, &archive_a, "foo", false)?;
    let child_b = spawn_install(files, &archive_b, "bar", false)?;
    let status_a = wait_child(child_a, "distinct-a")?;
    let status_b = wait_child(child_b, "distinct-b")?;
    if !status_a.success() || !status_b.success() {
        return Err(format!(
            "both distinct-alias installs should succeed, a={} b={}",
            status_a.success(),
            status_b.success()
        ));
    }
    let ledger = InstallLedger::load(files).map_err(|err| err.to_string())?;
    let aliases: Vec<_> = ledger
        .artifacts
        .iter()
        .map(|row| row.manifest_id.as_str())
        .collect();
    if !(aliases.contains(&"foo") && aliases.contains(&"bar")) {
        return Err(format!("expected foo and bar ledger rows, got {aliases:?}"));
    }
    if ledger.artifacts.len() != 2 {
        return Err(format!(
            "expected two ledger rows, got {}",
            ledger.artifacts.len()
        ));
    }
    Ok(())
}

/// Concurrent update and remove of one PluginKey serialize to a consistent namespace.
fn update_and_remove_serialize() -> Result<(), String> {
    let tmp = tempfile::tempdir().map_err(|err| err.to_string())?;
    let files = tmp.path();
    let (archive, digest) = make_named_archive(files, "foo.tar.gz", "foo");
    let plugins = files.join("plugins");
    let opts = InstallOptions {
        plugins_root: plugins,
        trust: TrustPolicy::allow_unverified_publisher(),
        ..Default::default()
    };
    let coord = PackageCoordinate {
        source: RegistrySource::LocalArchive,
        name: archive.display().to_string(),
        version: "1.0.0".into(),
    };
    let first = Installer::install_from_manifest(
        &test_manifest(
            "foo",
            digest,
            format!("file://{}", archive.display()),
            host_bookclerk_target(),
        ),
        &coord,
        &opts,
    )
    .map_err(|err| err.to_string())?;
    Installer::commit(&first).map_err(|err| err.to_string())?;
    let child_update = spawn_install(files, &archive, "foo", true)?;
    let child_remove = spawn_remove(files, "foo")?;
    let status_u = wait_child(child_update, "update")?;
    let status_r = wait_child(child_remove, "remove")?;
    if !status_u.success() && !status_r.success() {
        return Err("update vs remove: both failed".into());
    }
    let ledger = InstallLedger::load(files).map_err(|err| err.to_string())?;
    let trees = installed_plugin_dirs(&files.join("plugins"))?;
    let key = first.receipt.plugin_key().map_err(|err| err.to_string())?;
    let ledger_has = ledger.get(&key).is_some();
    match (trees.len(), ledger_has) {
        (0, false) | (1, true) => Ok(()),
        (n, has) => Err(format!(
            "inconsistent namespace after serialized update/remove: trees={n} ledger_has={has} statuses update={} remove={}",
            status_u.success(),
            status_r.success()
        )),
    }
}

/// A lock on host A must not delay an install on host B.
fn distinct_files_dirs_do_not_block() -> Result<(), String> {
    let host_a = tempfile::tempdir().map_err(|err| err.to_string())?;
    let host_b = tempfile::tempdir().map_err(|err| err.to_string())?;
    let (archive, _) = make_named_archive(host_b.path(), "foo.tar.gz", "foo");
    let hold = spawn_hold(host_a.path(), 2500)?;
    std::thread::sleep(Duration::from_millis(150));
    let start = Instant::now();
    let install = spawn_install(host_b.path(), &archive, "foo", false)?;
    let status = wait_child(install, "host-b-install")?;
    let elapsed = start.elapsed();
    let _ = wait_child(hold, "host-a-hold");
    if !status.success() {
        return Err("install on host B failed while host A held its lock".into());
    }
    if elapsed >= Duration::from_millis(1500) {
        return Err(format!(
            "host B install blocked on host A's lock (elapsed {elapsed:?}); locks must be files-dir local"
        ));
    }
    Ok(())
}

/// An install under the same files dir waits for a held mutation lock.
fn same_files_dir_blocks_while_held() -> Result<(), String> {
    let tmp = tempfile::tempdir().map_err(|err| err.to_string())?;
    let (archive, _) = make_named_archive(tmp.path(), "foo.tar.gz", "foo");
    let hold = spawn_hold(tmp.path(), 1500)?;
    std::thread::sleep(Duration::from_millis(150));
    let start = Instant::now();
    let install = spawn_install(tmp.path(), &archive, "foo", false)?;
    let status = wait_child(install, "same-dir-install")?;
    let elapsed = start.elapsed();
    let _ = wait_child(hold, "same-dir-hold");
    if !status.success() {
        return Err("install after lock holder exited should succeed".into());
    }
    if elapsed < Duration::from_millis(800) {
        return Err(format!(
            "same files-dir install returned too quickly ({elapsed:?}); expected to wait on the lock"
        ));
    }
    Ok(())
}

/// Spawns a child that installs `alias` from `archive` into `files_dir`.
fn spawn_install(
    files_dir: &Path,
    archive: &Path,
    alias: &str,
    replace: bool,
) -> Result<std::process::Child, String> {
    let mut cmd = child_cmd("install");
    cmd.env(FILES_DIR_ENV, files_dir)
        .env(ARCHIVE_ENV, archive)
        .env(ALIAS_ENV, alias);
    if replace {
        cmd.env(REPLACE_ENV, "1");
    }
    cmd.spawn().map_err(|err| err.to_string())
}

/// Spawns a child that removes `alias` from `files_dir`.
fn spawn_remove(files_dir: &Path, alias: &str) -> Result<std::process::Child, String> {
    child_cmd("remove")
        .env(FILES_DIR_ENV, files_dir)
        .env(ALIAS_ENV, alias)
        .spawn()
        .map_err(|err| err.to_string())
}

/// Spawns a child that holds the mutation lock on `files_dir`.
fn spawn_hold(files_dir: &Path, hold_ms: u64) -> Result<std::process::Child, String> {
    child_cmd("hold")
        .env(FILES_DIR_ENV, files_dir)
        .env(HOLD_MS_ENV, hold_ms.to_string())
        .spawn()
        .map_err(|err| err.to_string())
}

/// Waits for a child and includes its stderr when reporting failure.
fn wait_child(child: std::process::Child, label: &str) -> Result<std::process::ExitStatus, String> {
    let out = child.wait_with_output().map_err(|err| err.to_string())?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !stderr.trim().is_empty() {
            eprintln!("{label} stderr:\n{stderr}");
        }
    }
    Ok(out.status)
}

/// Test-harness command that re-execs this binary in child mode.
fn child_cmd(op: &str) -> Command {
    let exe = std::env::current_exe().expect("current test exe");
    let exe = exe.canonicalize().expect("canonicalize current test exe");
    assert!(exe.is_file(), "current test exe must be a regular file");
    let mut cmd = Command::new(exe);
    cmd.env(CHILD_ENV, op)
        .env("RUST_BACKTRACE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// Directory names under `plugins/` that contain `plugin.toml`.
fn installed_plugin_dirs(plugins: &Path) -> Result<Vec<String>, String> {
    let plugins = bookclerk_plugin_catalog::require_under(plugins, plugins)
        .or_else(|_| {
            plugins
                .canonicalize()
                .map_err(|err| bookclerk_plugin_catalog::CatalogError::message(err.to_string()))
        })
        .map_err(|err| err.to_string())?;
    if !plugins.is_dir() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in fs::read_dir(&plugins).map_err(|err| err.to_string())? {
        let entry = entry.map_err(|err| err.to_string())?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == ".staging" {
            continue;
        }
        let toml =
            bookclerk_plugin_catalog::require_under(&plugins, &entry.path().join("plugin.toml"))
                .map_err(|err| err.to_string())?;
        if toml.is_file() {
            names.push(name.into_owned());
        }
    }
    names.sort();
    Ok(names)
}

/// Minimal package manifest for a local-archive install fixture.
fn test_manifest(id: &str, digest: String, url: String, target: &str) -> BookclerkPackageManifest {
    BookclerkPackageManifest {
        schema_version: 1,
        protocol: None,
        api_version: 1,
        api_version_max: None,
        min_bookclerk: None,
        kind: PluginKind::Integration,
        id: id.into(),
        display_name: Some("Echo".into()),
        description: None,
        coordinate: None,
        artifacts: vec![ArtifactTarget {
            target: target.into(),
            url,
            archive_sha256: digest,
            archive_root: ".".into(),
            executable: "echo".into(),
            executable_sha256: None,
        }],
        sandbox: Default::default(),
        links: Default::default(),
        yanked: false,
        released_at: None,
        publisher: None,
    }
}

/// Writes a tiny gzipped plugin archive and returns `(path, sha256)`.
fn make_named_archive(dir: &Path, filename: &str, id: &str) -> (PathBuf, String) {
    let archive = dir.join(filename);
    {
        let file = fs::File::create(&archive).unwrap();
        let enc = GzEncoder::new(file, Compression::default());
        let mut tar = Builder::new(enc);
        let toml = format!(
            "api_version = 3\nid = \"{id}\"\ncommand = \"./echo\"\nentrypoints = [\"cli\"]\n\
             [capabilities.network]\nmode = \"outbound\"\n"
        );
        let toml = toml.into_bytes();
        let mut h = tar::Header::new_gnu();
        h.set_size(toml.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        tar.append_data(&mut h, "plugin.toml", toml.as_slice())
            .unwrap();
        let bin = b"#!/bin/sh\necho ok\n";
        let mut h2 = tar::Header::new_gnu();
        h2.set_size(bin.len() as u64);
        h2.set_mode(0o755);
        h2.set_cksum();
        tar.append_data(&mut h2, "echo", &bin[..]).unwrap();
        let enc = tar.into_inner().unwrap();
        enc.finish().unwrap();
    }
    let digest = bookclerk_plugin_catalog::sha256_file(&archive).unwrap();
    (archive, digest)
}
