//! Release packaging aligned with `docs/plugin-registry.md` artifact naming.

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use flate2::write::GzEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

use crate::plugins::{self, PLATFORM_PLUGIN_IDS, STAGE_SPECS};

const HOST_BINARIES: &[(&str, &str)] = &[
    ("bookclerk-cli", "bookclerk"),
    ("bookclerkd", "bookclerkd"),
    ("bookclerk-jail", "bookclerk-jail"),
    ("bookclerk-media-worker", "bookclerk-media-worker"),
];

/// Host target triple for the machine running the packager.
#[must_use]
pub fn host_target_triple() -> &'static str {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "x86_64-pc-windows-msvc"
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        "aarch64-pc-windows-msvc"
    } else {
        "unknown"
    }
}

pub fn package_plugins(root: &Path, out_dir: &Path, version: &str) -> Result<()> {
    let staged = root.join("target").join("plugin-artifacts-pack");
    plugins::stage(root, true, &staged)?;
    fs::create_dir_all(out_dir).with_context(|| format!("create {}", out_dir.display()))?;

    let target = host_target_triple();
    let mut checksums = String::new();
    for spec in STAGE_SPECS {
        let plugin_dir = staged.join(spec.id);
        let crate_name = plugins::crate_name(spec);
        let archive = archive_path(out_dir, &crate_name, version, target);
        write_plugin_archive(&plugin_dir, &archive)?;
        let digest = sha256_file(&archive)?;
        checksums.push_str(&format!(
            "{digest}  {}\n",
            archive.file_name().unwrap().to_string_lossy()
        ));
        eprintln!("packaged {} -> {}", crate_name, archive.display());
    }
    let sums_path = out_dir.join("SHA256SUMS");
    fs::write(&sums_path, checksums).with_context(|| format!("write {}", sums_path.display()))?;
    eprintln!("wrote {}", sums_path.display());
    Ok(())
}

pub fn package_hosts(root: &Path, out_dir: &Path, version: &str) -> Result<()> {
    ensure_ui_built(root)?;
    build_hosts(root)?;
    let bundle = out_dir.join(bundle_dir_name(version));
    if bundle.exists() {
        fs::remove_dir_all(&bundle).with_context(|| format!("remove {}", bundle.display()))?;
    }
    fs::create_dir_all(&bundle).with_context(|| format!("create {}", bundle.display()))?;

    let bin_dir = root.join("target").join("release");
    for (_pkg, dest_name) in HOST_BINARIES {
        let src = resolve_host_binary(&bin_dir, dest_name)?;
        let dest = bundle.join(exe_name(dest_name));
        fs::copy(&src, &dest)
            .with_context(|| format!("copy {} -> {}", src.display(), dest.display()))?;
        set_executable(&dest)?;
        eprintln!("copied {} -> {}", src.display(), dest.display());
    }

    let archive = archive_path(out_dir, "bookclerk", version, host_target_triple());
    write_dir_archive(&bundle, &archive)?;
    let digest = sha256_file(&archive)?;
    let sums_path = out_dir.join(format!(
        "SHA256SUMS-bookclerk-{}-{}",
        version,
        host_target_triple()
    ));
    fs::write(
        &sums_path,
        format!(
            "{digest}  {}\n",
            archive.file_name().unwrap().to_string_lossy()
        ),
    )
    .with_context(|| format!("write {}", sums_path.display()))?;
    eprintln!("packaged hosts -> {}", archive.display());
    Ok(())
}

pub fn package_platform(root: &Path, out_dir: &Path, version: &str) -> Result<()> {
    ensure_ui_built(root)?;
    build_hosts(root)?;
    let staged = root.join("target").join("plugin-artifacts-pack");
    plugins::stage(root, true, &staged)?;

    let bundle = out_dir.join(bundle_dir_name(version));
    if bundle.exists() {
        fs::remove_dir_all(&bundle).with_context(|| format!("remove {}", bundle.display()))?;
    }
    fs::create_dir_all(&bundle).with_context(|| format!("create {}", bundle.display()))?;

    let bin_dir = root.join("target").join("release");
    for (_pkg, dest_name) in HOST_BINARIES {
        let src = resolve_host_binary(&bin_dir, dest_name)?;
        let dest = bundle.join(exe_name(dest_name));
        fs::copy(&src, &dest)
            .with_context(|| format!("copy {} -> {}", src.display(), dest.display()))?;
        set_executable(&dest)?;
    }

    let plugins_root = bundle.join("plugins");
    fs::create_dir_all(&plugins_root)
        .with_context(|| format!("create {}", plugins_root.display()))?;
    for id in PLATFORM_PLUGIN_IDS {
        copy_dir_all(&staged.join(id), &plugins_root.join(id))?;
        eprintln!("bundled platform plugin `{id}`");
    }

    let archive = archive_path(out_dir, "bookclerk-platform", version, host_target_triple());
    write_dir_archive(&bundle, &archive)?;
    let digest = sha256_file(&archive)?;
    let sums_path = out_dir.join(format!(
        "SHA256SUMS-bookclerk-platform-{}-{}",
        version,
        host_target_triple()
    ));
    fs::write(
        &sums_path,
        format!(
            "{digest}  {}\n",
            archive.file_name().unwrap().to_string_lossy()
        ),
    )
    .with_context(|| format!("write {}", sums_path.display()))?;
    eprintln!("packaged platform -> {}", archive.display());
    Ok(())
}

fn bundle_dir_name(version: &str) -> String {
    format!("bookclerk-{}-{}", version, host_target_triple())
}

fn archive_path(out_dir: &Path, crate_name: &str, version: &str, target: &str) -> PathBuf {
    let ext = if cfg!(target_os = "windows") {
        "zip"
    } else {
        "tar.gz"
    };
    out_dir.join(format!("{crate_name}-{version}-{target}.{ext}"))
}

fn write_plugin_archive(plugin_dir: &Path, archive: &Path) -> Result<()> {
    write_dir_archive(plugin_dir, archive)
}

fn write_dir_archive(src_dir: &Path, archive: &Path) -> Result<()> {
    if cfg!(target_os = "windows") {
        write_zip_dir(src_dir, archive)
    } else {
        write_tar_gz_dir(src_dir, archive)
    }
}

fn write_tar_gz_dir(src_dir: &Path, archive: &Path) -> Result<()> {
    let file = File::create(archive).with_context(|| format!("create {}", archive.display()))?;
    let enc = GzEncoder::new(file, Compression::default());
    let mut tar = tar::Builder::new(enc);
    for entry in WalkDir::new(src_dir) {
        let entry = entry.with_context(|| format!("walk {}", src_dir.display()))?;
        let path = entry.path();
        let rel = path.strip_prefix(src_dir).context("strip archive prefix")?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        if entry.file_type().is_dir() {
            tar.append_dir(rel, path)
                .with_context(|| format!("tar dir {}", path.display()))?;
        } else if entry.file_type().is_file() {
            tar.append_path_with_name(path, rel)
                .with_context(|| format!("tar file {}", path.display()))?;
        }
    }
    tar.finish().context("finish tar archive")?;
    Ok(())
}

fn write_zip_dir(src_dir: &Path, archive: &Path) -> Result<()> {
    let file = File::create(archive).with_context(|| format!("create {}", archive.display()))?;
    let mut zip = ZipWriter::new(BufWriter::new(file));
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for entry in WalkDir::new(src_dir) {
        let entry = entry.with_context(|| format!("walk {}", src_dir.display()))?;
        let path = entry.path();
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = path
            .strip_prefix(src_dir)
            .context("strip zip prefix")?
            .to_string_lossy()
            .replace('\\', "/");
        zip.start_file(rel, options).context("zip start_file")?;
        let mut reader = File::open(path).with_context(|| format!("open {}", path.display()))?;
        std::io::copy(&mut reader, &mut zip).context("zip write")?;
    }
    zip.finish().context("finish zip")?;
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn ensure_ui_built(root: &Path) -> Result<()> {
    let ui_dist = root.join("ui").join("dist");
    if ui_dist.join("index.html").is_file() {
        return Ok(());
    }
    bail!(
        "ui/dist missing — run `cd ui && npm ci && npm run build` before packaging hosts \
         (bookclerkd embeds the web UI)"
    );
}

fn build_hosts(root: &Path) -> Result<()> {
    let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.current_dir(root);
    cmd.arg("build");
    cmd.arg("--release");
    for (pkg, _) in HOST_BINARIES {
        cmd.args(["-p", pkg]);
    }
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = cmd.status().context("cargo build release hosts")?;
    if status.success() {
        Ok(())
    } else {
        bail!("cargo build release hosts exited with {status}");
    }
}

fn resolve_host_binary(bin_dir: &Path, name: &str) -> Result<PathBuf> {
    let plain = bin_dir.join(exe_name(name));
    if plain.is_file() {
        return Ok(plain);
    }
    bail!("missing host binary: {}", plain.display());
}

fn exe_name(base: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

fn copy_dir_all(src: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    for entry in WalkDir::new(src) {
        let entry = entry.with_context(|| format!("walk {}", src.display()))?;
        let path = entry.path();
        let rel = path.strip_prefix(src).context("strip copy prefix")?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = dest.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&out).with_context(|| format!("create {}", out.display()))?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            fs::copy(path, &out)
                .with_context(|| format!("copy {} -> {}", path.display(), out.display()))?;
            set_executable(&out)?;
        }
    }
    Ok(())
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
    fn archive_names_follow_registry() {
        let path = archive_path(
            Path::new("/out"),
            "bookclerk-plugin-source-audible",
            "0.1.0",
            "x86_64-unknown-linux-gnu",
        );
        let name = path.file_name().unwrap().to_string_lossy();
        if cfg!(target_os = "windows") {
            assert!(name.ends_with(".zip"));
        } else {
            assert!(name.ends_with(".tar.gz"));
        }
        assert!(name.contains("bookclerk-plugin-source-audible-0.1.0-"));
    }
}
