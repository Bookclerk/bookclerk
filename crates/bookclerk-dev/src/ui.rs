//! Rebuild `ui/dist` when the SPA sources are newer than the last Vite build.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use walkdir::WalkDir;

/// Rebuilds `ui/dist` when `index.html` is missing or older than SPA sources.
///
/// `cargo dev` previously compiled only Rust crates, so a `git pull` that
/// changed `ui/src` still served the previous Vite output from `ui/dist`.
///
/// # Arguments
///
/// * `root` - Cargo workspace root (the directory that contains `ui/`).
///
/// # Errors
///
/// Returns an error when `npm ci` / `npm run build` fail, `npm` is missing, or
/// `ui/dist/index.html` is still absent after the build.
pub fn ensure_ui_dist(root: &Path) -> Result<()> {
    let ui = root.join("ui");
    if !ui.join("package.json").is_file() {
        bail!("ui/package.json missing under {}", root.display());
    }
    if !ui_dist_is_stale(&ui) {
        return Ok(());
    }
    if npm_install_needed(&ui) {
        run_npm(&ui, &["ci"])?;
    }
    run_npm(&ui, &["run", "build"])?;
    if !ui.join("dist").join("index.html").is_file() {
        bail!(
            "ui/dist/index.html missing after npm run build ({})",
            ui.display()
        );
    }
    eprintln!("built ui/dist");
    Ok(())
}

/// True when `node_modules` is missing or older than `package-lock.json`.
///
/// `npm ci` writes `node_modules/.package-lock.json`. A lockfile newer than
/// that snapshot means dependencies were added (or the tree is from an older
/// image) and a directory check alone would skip install.
fn npm_install_needed(ui: &Path) -> bool {
    let installed = ui.join("node_modules").join(".package-lock.json");
    let Ok(installed_mtime) = installed.metadata().and_then(|meta| meta.modified()) else {
        return true;
    };
    file_is_newer(&ui.join("package-lock.json"), installed_mtime)
}

/// True when `ui/dist/index.html` is missing or older than watched SPA inputs.
fn ui_dist_is_stale(ui: &Path) -> bool {
    let dist_index = ui.join("dist").join("index.html");
    let Ok(dist_mtime) = dist_index.metadata().and_then(|meta| meta.modified()) else {
        return true;
    };
    watched_ui_paths(ui)
        .iter()
        .any(|path| path_is_newer(path, dist_mtime))
}

/// `ui/` files and directories whose mtime should trigger a Vite rebuild.
fn watched_ui_paths(ui: &Path) -> Vec<std::path::PathBuf> {
    [
        "src",
        "public",
        "index.html",
        "package.json",
        "package-lock.json",
        "vite.config.ts",
        "tsconfig.json",
        "tsconfig.app.json",
        "tsconfig.node.json",
    ]
    .into_iter()
    .map(|name| ui.join(name))
    .filter(|path| path.exists())
    .collect()
}

/// True when `path` (or any file under it) is newer than `dist_mtime`.
fn path_is_newer(path: &Path, dist_mtime: SystemTime) -> bool {
    if path.is_file() {
        return file_is_newer(path, dist_mtime);
    }
    WalkDir::new(path)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .any(|entry| file_is_newer(entry.path(), dist_mtime))
}

/// True when `path`'s mtime is strictly after `dist_mtime`.
fn file_is_newer(path: &Path, dist_mtime: SystemTime) -> bool {
    path.metadata()
        .and_then(|meta| meta.modified())
        .is_ok_and(|mtime| mtime > dist_mtime)
}

/// Runs `npm` with `args` in `ui`, inheriting stdio.
fn run_npm(ui: &Path, args: &[&str]) -> Result<()> {
    let mut cmd = Command::new(npm_program());
    cmd.args(args)
        .current_dir(ui)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = cmd
        .status()
        .with_context(|| format!("{} {} (in {})", npm_program(), args.join(" "), ui.display()))?;
    if !status.success() {
        bail!("{} {} exited with {status}", npm_program(), args.join(" "));
    }
    Ok(())
}

/// `npm.cmd` on Windows so `std::process::Command` finds the npm shim.
fn npm_program() -> &'static str {
    if cfg!(windows) {
        "npm.cmd"
    } else {
        "npm"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    #[test]
    fn missing_dist_is_stale() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ui = tmp.path().join("ui");
        fs::create_dir_all(ui.join("src")).unwrap();
        fs::write(ui.join("package.json"), "{}").unwrap();
        fs::write(ui.join("src").join("main.tsx"), "export {}").unwrap();
        assert!(ui_dist_is_stale(&ui));
    }

    #[test]
    fn newer_source_is_stale() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ui = tmp.path().join("ui");
        fs::create_dir_all(ui.join("src")).unwrap();
        fs::create_dir_all(ui.join("dist")).unwrap();
        fs::write(ui.join("package.json"), "{}").unwrap();
        fs::write(ui.join("dist").join("index.html"), "<html></html>").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        fs::write(ui.join("src").join("main.tsx"), "export {}").unwrap();
        assert!(ui_dist_is_stale(&ui));
    }

    #[test]
    fn dist_newer_than_sources_is_fresh() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ui = tmp.path().join("ui");
        fs::create_dir_all(ui.join("src")).unwrap();
        fs::create_dir_all(ui.join("dist")).unwrap();
        fs::write(ui.join("package.json"), "{}").unwrap();
        fs::write(ui.join("src").join("main.tsx"), "export {}").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        fs::write(ui.join("dist").join("index.html"), "<html></html>").unwrap();
        assert!(!ui_dist_is_stale(&ui));
    }

    #[test]
    fn npm_install_needed_when_node_modules_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ui = tmp.path().join("ui");
        fs::create_dir_all(&ui).unwrap();
        fs::write(ui.join("package-lock.json"), "{}").unwrap();
        assert!(npm_install_needed(&ui));
    }

    #[test]
    fn npm_install_needed_when_lockfile_newer_than_installed_tree() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ui = tmp.path().join("ui");
        fs::create_dir_all(ui.join("node_modules")).unwrap();
        fs::write(ui.join("node_modules").join(".package-lock.json"), "{}").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        fs::write(ui.join("package-lock.json"), "{\"lockfileVersion\":3}").unwrap();
        assert!(npm_install_needed(&ui));
    }

    #[test]
    fn npm_install_not_needed_when_installed_lock_is_current() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ui = tmp.path().join("ui");
        fs::create_dir_all(ui.join("node_modules")).unwrap();
        fs::write(ui.join("package-lock.json"), "{}").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        fs::write(ui.join("node_modules").join(".package-lock.json"), "{}").unwrap();
        assert!(!npm_install_needed(&ui));
    }
}
