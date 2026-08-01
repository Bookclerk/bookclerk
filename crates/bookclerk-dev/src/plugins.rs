//! Build and stage first-party external plugin artifacts.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

/// One staged plugin directory under the artifacts root.
#[derive(Debug, Clone, Copy)]
pub struct StageSpec {
    /// Plugin kind segment in the crates.io crate name.
    pub kind: &'static str,
    /// Cargo package to build (may repeat across entries).
    pub package: &'static str,
    /// Binary name in `target/<profile>/`.
    pub bin_name: &'static str,
    /// Plugin id (install folder name).
    pub id: &'static str,
    /// Crate path relative to workspace root.
    pub srcdir: &'static str,
    /// Manifest file name inside `srcdir`.
    pub manifest: &'static str,
}

/// All first-party plugins staged for dev/CI.
pub const STAGE_SPECS: &[StageSpec] = &[
    StageSpec {
        kind: "integration",
        package: "bookclerk-plugin-echo-integration",
        bin_name: "bookclerk-plugin-echo-integration",
        id: "echo",
        srcdir: "crates/bookclerk-plugin-examples/echo-integration",
        manifest: "plugin.toml",
    },
    StageSpec {
        kind: "source",
        package: "bookclerk-plugin-source-audible",
        bin_name: "bookclerk-plugin-source-audible",
        id: "audible",
        srcdir: "crates/bookclerk-plugins/source-audible",
        manifest: "plugin.toml",
    },
    StageSpec {
        kind: "source",
        package: "bookclerk-plugin-source-libro",
        bin_name: "bookclerk-plugin-source-libro",
        id: "libro",
        srcdir: "crates/bookclerk-plugins/source-libro",
        manifest: "plugin.toml",
    },
    StageSpec {
        kind: "source",
        package: "bookclerk-plugin-source-chirp",
        bin_name: "bookclerk-plugin-source-chirp",
        id: "chirp",
        srcdir: "crates/bookclerk-plugins/source-chirp",
        manifest: "plugin.toml",
    },
    StageSpec {
        kind: "source",
        package: "bookclerk-plugin-source-graphicaudio",
        bin_name: "bookclerk-plugin-source-graphicaudio",
        id: "graphicaudio",
        srcdir: "crates/bookclerk-plugins/source-graphicaudio",
        manifest: "plugin.toml",
    },
    StageSpec {
        kind: "integration",
        package: "bookclerk-plugin-integration-audiobookshelf",
        bin_name: "bookclerk-plugin-integration-audiobookshelf",
        id: "audiobookshelf",
        srcdir: "crates/bookclerk-plugins/integration-audiobookshelf",
        manifest: "plugin.toml",
    },
    StageSpec {
        kind: "output",
        package: "bookclerk-plugin-destination-s3",
        bin_name: "bookclerk-plugin-destination-s3",
        id: "s3",
        srcdir: "crates/bookclerk-plugins/destination-s3",
        manifest: "plugin.toml",
    },
    StageSpec {
        kind: "output",
        package: "bookclerk-plugin-destination-local",
        bin_name: "bookclerk-plugin-destination-local",
        id: "local",
        srcdir: "crates/bookclerk-plugins/destination-local",
        manifest: "plugin.toml",
    },
    StageSpec {
        kind: "database",
        package: "bookclerk-plugin-database",
        bin_name: "bookclerk-plugin-database",
        id: "sqlite",
        srcdir: "crates/bookclerk-plugins/database",
        manifest: "plugin.toml",
    },
    StageSpec {
        kind: "database",
        package: "bookclerk-plugin-database",
        bin_name: "bookclerk-plugin-database",
        id: "d1",
        srcdir: "crates/bookclerk-plugins/database",
        manifest: "plugin-d1.toml",
    },
    StageSpec {
        kind: "database",
        package: "bookclerk-plugin-database",
        bin_name: "bookclerk-plugin-database",
        id: "postgres",
        srcdir: "crates/bookclerk-plugins/database",
        manifest: "plugin-postgres.toml",
    },
];

/// crates.io crate name for a staged plugin (`bookclerk-plugin-{kind}-{id}`).
#[must_use]
pub fn crate_name(spec: &StageSpec) -> String {
    format!("bookclerk-plugin-{}-{}", spec.kind, spec.id)
}

/// Platform-shipped plugins bundled with host releases (not optional storefronts).
pub const PLATFORM_PLUGIN_IDS: &[&str] = &["sqlite", "local"];

pub fn build(root: &Path, release: bool) -> Result<()> {
    let packages = unique_packages();
    let mut cmd = cargo(root);
    cmd.arg("build");
    if release {
        cmd.arg("--release");
    }
    for pkg in packages {
        cmd.args(["-p", pkg]);
    }
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = cmd.status().context("cargo build first-party plugins")?;
    if !status.success() {
        bail!("cargo build first-party plugins exited with {status}");
    }
    let profile = profile_dir(release);
    eprintln!(
        "built first-party plugins ({profile}) under {}/target/{profile}/",
        root.display()
    );
    Ok(())
}

pub fn stage(root: &Path, release: bool, dest: &Path) -> Result<()> {
    build(root, release)?;
    fs::create_dir_all(dest).with_context(|| format!("create staging dir {}", dest.display()))?;

    let bin_dir = root.join("target").join(profile_dir(release));
    for spec in STAGE_SPECS {
        stage_one(root, &bin_dir, dest, spec)?;
    }
    eprintln!("BOOKCLERK_PLUGIN_ARTIFACTS={}", dest.display());
    Ok(())
}

fn stage_one(root: &Path, bin_dir: &Path, dest: &Path, spec: &StageSpec) -> Result<()> {
    let src_bin = resolve_binary(bin_dir, spec.bin_name)?;
    let out = dest.join(spec.id);
    // Clear stale binaries/resources from a previous stage of this id.
    if out.exists() {
        fs::remove_dir_all(&out).with_context(|| format!("clear staging dir {}", out.display()))?;
    }
    fs::create_dir_all(&out).with_context(|| format!("create {}", out.display()))?;

    let dest_bin = out.join(src_bin.file_name().context("binary has no file name")?);
    fs::copy(&src_bin, &dest_bin)
        .with_context(|| format!("copy {} -> {}", src_bin.display(), dest_bin.display()))?;
    set_executable(&dest_bin)?;

    let manifest_src = root.join(spec.srcdir).join(spec.manifest);
    if !manifest_src.is_file() {
        bail!(
            "missing manifest for {}: {}",
            spec.id,
            manifest_src.display()
        );
    }
    let manifest_dest = out.join("plugin.toml");
    fs::copy(&manifest_src, &manifest_dest).with_context(|| {
        format!(
            "copy {} -> {}",
            manifest_src.display(),
            manifest_dest.display()
        )
    })?;
    patch_command(
        &manifest_dest,
        dest_bin.file_name().and_then(|n| n.to_str()).unwrap(),
    )?;

    eprintln!("staged {} -> {}", spec.id, out.display());
    Ok(())
}

fn resolve_binary(bin_dir: &Path, bin_name: &str) -> Result<PathBuf> {
    let plain = bin_dir.join(bin_name);
    if plain.is_file() {
        return Ok(plain);
    }
    let exe = bin_dir.join(format!("{bin_name}.exe"));
    if exe.is_file() {
        return Ok(exe);
    }
    bail!(
        "missing binary: {} (run `cargo build-plugins`)",
        plain.display()
    );
}

/// Rewrite the `command =` line to point at the staged binary basename.
pub fn patch_command(manifest_path: &Path, bin_name: &str) -> Result<()> {
    let text = fs::read_to_string(manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;
    let mut lines = Vec::new();
    for line in text.lines() {
        if line.trim_start().starts_with("command") {
            lines.push(format!("command = \"./{bin_name}\""));
        } else {
            lines.push(line.to_string());
        }
    }
    let patched = if text.ends_with('\n') || lines.is_empty() {
        format!("{}\n", lines.join("\n"))
    } else {
        lines.join("\n")
    };
    fs::write(manifest_path, patched)
        .with_context(|| format!("write {}", manifest_path.display()))?;
    Ok(())
}

fn unique_packages() -> Vec<&'static str> {
    let mut seen = HashSet::new();
    STAGE_SPECS
        .iter()
        .filter_map(|spec| {
            if seen.insert(spec.package) {
                Some(spec.package)
            } else {
                None
            }
        })
        .collect()
}

fn profile_dir(release: bool) -> &'static str {
    if release {
        "release"
    } else {
        "debug"
    }
}

fn cargo(root: &Path) -> Command {
    let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.current_dir(root);
    cmd
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)
        .with_context(|| format!("metadata {}", path.display()))?
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).with_context(|| format!("chmod {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_command_rewrites_command_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugin.toml");
        fs::write(
            &path,
            "id = \"x\"\ncommand = \"./old\"\nkind = \"source\"\n",
        )
        .expect("write");
        patch_command(&path, "bookclerk-plugin-x").expect("patch");
        let text = fs::read_to_string(&path).expect("read");
        assert!(text.contains("command = \"./bookclerk-plugin-x\""));
        assert!(!text.contains("./old"));
    }

    #[test]
    fn unique_packages_dedupes_database_crate() {
        let pkgs = unique_packages();
        assert_eq!(
            pkgs.iter()
                .filter(|p| **p == "bookclerk-plugin-database")
                .count(),
            1
        );
        assert!(pkgs.contains(&"bookclerk-plugin-source-audible"));
    }
}
