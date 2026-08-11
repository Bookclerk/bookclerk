//! Release packaging aligned with `docs/plugin-registry.md` artifact naming.

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use flate2::write::GzEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

use crate::plugins::{self, BuildSelection};

/// `(cargo package, on-disk binary name)` for host + helper archives from default-members.
fn host_binaries(root: &Path) -> Result<Vec<(String, String)>> {
    let pkgs = plugins::packages_for(
        root,
        BuildSelection {
            platform: true,
            ..Default::default()
        },
    )?;
    // Only hosts/helpers — not platform plugin guest packages.
    let guest_pkgs: std::collections::HashSet<_> = plugins::discover_platform(root)?
        .into_iter()
        .filter_map(|g| g.package)
        .collect();
    let mut out = Vec::new();
    for pkg in pkgs {
        if guest_pkgs.contains(&pkg) {
            continue;
        }
        let bin = match pkg.as_str() {
            "bookclerk-cli" => "bookclerk".to_string(),
            other => other.to_string(),
        };
        out.push((pkg, bin));
    }
    Ok(out)
}

/// Host rustc target triple for the machine running the packager.
///
/// Prefer [`bookclerk_target`] for new archive filenames. This remains for
/// callers that still speak rustc triples; [`normalize_bookclerk_target`] maps
/// either form onto a Bookclerk target name.
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

/// Canonical Bookclerk artifact target for the host (e.g. `linux-x64-gnu`).
#[must_use]
pub fn bookclerk_target() -> &'static str {
    normalize_bookclerk_target(host_target_triple()).unwrap_or("unknown")
}

/// Map a Bookclerk target or legacy rustc triple to a Bookclerk target name.
///
/// # Arguments
///
/// * `triple_or_target` - String `triple_or_target` for this call.
///
/// # Returns
///
/// `Some(...)` when found / applicable; otherwise `None`.
#[must_use]
pub fn normalize_bookclerk_target(triple_or_target: &str) -> Option<&'static str> {
    match triple_or_target {
        "linux-x64-gnu" | "x86_64-unknown-linux-gnu" => Some("linux-x64-gnu"),
        "linux-arm64-gnu" | "aarch64-unknown-linux-gnu" => Some("linux-arm64-gnu"),
        "macos-x64" | "x86_64-apple-darwin" => Some("macos-x64"),
        "macos-arm64" | "aarch64-apple-darwin" => Some("macos-arm64"),
        "windows-x64" | "x86_64-pc-windows-msvc" => Some("windows-x64"),
        "windows-arm64" | "aarch64-pc-windows-msvc" => Some("windows-arm64"),
        _ => None,
    }
}

/// Whether archives for this Bookclerk target (or rustc triple) use `.zip`.
///
/// # Arguments
///
/// * `triple_or_target` - String `triple_or_target` for this call.
///
/// # Returns
///
/// `true` when the predicate holds.
#[must_use]
pub fn uses_zip_archive(triple_or_target: &str) -> bool {
    match normalize_bookclerk_target(triple_or_target) {
        Some(t) => t.starts_with("windows-"),
        None => cfg!(target_os = "windows"),
    }
}

/// Packages discovered plugin guests into the release artifacts directory.
///
/// # Arguments
///
/// * `root` - Cargo workspace root directory.
/// * `out_dir` - Filesystem path (`out_dir`).
/// * `version` - String `version` for this call.
///
/// # Returns
///
/// The successful result value for this operation.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn package_plugins(root: &Path, out_dir: &Path, version: &str) -> Result<()> {
    let staged = root.join("target").join("plugin-artifacts-pack");
    plugins::stage_optional_for_pack(root, &staged, true)?;
    fs::create_dir_all(out_dir).with_context(|| format!("create {}", out_dir.display()))?;

    let target = bookclerk_target();
    let mut checksums = String::new();
    for guest in plugins::discover_optional(root)? {
        let plugin_dir = staged.join(&guest.id);
        let crate_name = guest
            .package
            .clone()
            .unwrap_or_else(|| format!("bookclerk-plugin-{}", guest.id));
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

/// Packages host binaries (`bookclerk`, `bookclerkd`, helpers) into artifacts.
///
/// # Arguments
///
/// * `root` - Cargo workspace root directory.
/// * `out_dir` - Filesystem path (`out_dir`).
/// * `version` - String `version` for this call.
///
/// # Returns
///
/// The successful result value for this operation.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn package_hosts(root: &Path, out_dir: &Path, version: &str) -> Result<()> {
    ensure_ui_built(root)?;
    build_hosts(root)?;
    let bundle = out_dir.join(bundle_dir_name(version));
    if bundle.exists() {
        fs::remove_dir_all(&bundle).with_context(|| format!("remove {}", bundle.display()))?;
    }
    fs::create_dir_all(&bundle).with_context(|| format!("create {}", bundle.display()))?;

    let bin_dir = root.join("target").join("release");
    for (_pkg, dest_name) in host_binaries(root)? {
        let src = resolve_host_binary(&bin_dir, &dest_name)?;
        let dest = bundle.join(exe_name(&dest_name));
        fs::copy(&src, &dest)
            .with_context(|| format!("copy {} -> {}", src.display(), dest.display()))?;
        set_executable(&dest)?;
        eprintln!("copied {} -> {}", src.display(), dest.display());
    }
    copy_pinned_workerd(root, &bundle)?;

    let archive = archive_path(out_dir, "bookclerk", version, bookclerk_target());
    write_dir_archive(&bundle, &archive)?;
    let digest = sha256_file(&archive)?;
    let sums_path = out_dir.join(format!(
        "SHA256SUMS-bookclerk-{}-{}",
        version,
        bookclerk_target()
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

/// Packages platform-only guests (sqlite / local) into artifacts.
///
/// # Arguments
///
/// * `root` - Cargo workspace root directory.
/// * `out_dir` - Filesystem path (`out_dir`).
/// * `version` - String `version` for this call.
///
/// # Returns
///
/// The successful result value for this operation.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn package_platform(root: &Path, out_dir: &Path, version: &str) -> Result<()> {
    ensure_ui_built(root)?;
    plugins::build_selection(
        root,
        true,
        BuildSelection {
            platform: true,
            ..Default::default()
        },
    )?;
    let staged = root.join("target").join("plugin-artifacts-pack-platform");
    plugins::stage_platform_for_pack(root, &staged, true, true)?;

    let bundle = out_dir.join(bundle_dir_name(version));
    if bundle.exists() {
        fs::remove_dir_all(&bundle).with_context(|| format!("remove {}", bundle.display()))?;
    }
    fs::create_dir_all(&bundle).with_context(|| format!("create {}", bundle.display()))?;

    let bin_dir = root.join("target").join("release");
    for (_pkg, dest_name) in host_binaries(root)? {
        let src = resolve_host_binary(&bin_dir, &dest_name)?;
        let dest = bundle.join(exe_name(&dest_name));
        fs::copy(&src, &dest)
            .with_context(|| format!("copy {} -> {}", src.display(), dest.display()))?;
        set_executable(&dest)?;
    }
    copy_pinned_workerd(root, &bundle)?;

    let plugins_root = bundle.join("plugins");
    fs::create_dir_all(&plugins_root)
        .with_context(|| format!("create {}", plugins_root.display()))?;
    for guest in plugins::discover_platform(root)? {
        copy_dir_all(&staged.join(&guest.id), &plugins_root.join(&guest.id))?;
        eprintln!("bundled platform plugin `{}`", guest.id);
    }

    let archive = archive_path(out_dir, "bookclerk-platform", version, bookclerk_target());
    write_dir_archive(&bundle, &archive)?;
    let digest = sha256_file(&archive)?;
    let sums_path = out_dir.join(format!(
        "SHA256SUMS-bookclerk-platform-{}-{}",
        version,
        bookclerk_target()
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
    format!("bookclerk-{}-{}", version, bookclerk_target())
}

fn copy_pinned_workerd(root: &Path, bundle: &Path) -> Result<()> {
    let workerd = crate::ensure_workerd_for_profile(root, true)?;
    let dest_name = bookclerk_workerd::binary_name();
    let dest = bundle.join(dest_name);
    fs::copy(&workerd, &dest)
        .with_context(|| format!("copy {} -> {}", workerd.display(), dest.display()))?;
    set_executable(&dest)?;
    let stamp_src = workerd
        .parent()
        .map(|d| d.join(bookclerk_workerd::WORKERD_VERSION_STAMP));
    if let Some(src) = stamp_src {
        if src.is_file() {
            let _ = fs::copy(&src, bundle.join(bookclerk_workerd::WORKERD_VERSION_STAMP));
        }
    }
    eprintln!("copied pinned workerd -> {}", dest.display());
    Ok(())
}

/// Archive path using a Bookclerk target or legacy rustc triple in the filename.
///
/// New packaging prefers Bookclerk names (`linux-x64-gnu`); legacy triples are
/// still accepted so older docs/CI keep working during the transition.
fn archive_path(out_dir: &Path, crate_name: &str, version: &str, target: &str) -> PathBuf {
    let target = normalize_bookclerk_target(target).unwrap_or(target);
    let ext = if uses_zip_archive(target) {
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
    Ok(hex::encode(Sha256::digest(bytes)))
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
    plugins::build_selection(
        root,
        true,
        BuildSelection {
            platform: true,
            ..Default::default()
        },
    )
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
    fn archive_names_prefer_bookclerk_target() {
        let path = archive_path(
            Path::new("/out"),
            "bookclerk-plugin-source-audible",
            "0.1.0",
            "linux-x64-gnu",
        );
        let name = path.file_name().unwrap().to_string_lossy();
        assert_eq!(
            name.as_ref(),
            "bookclerk-plugin-source-audible-0.1.0-linux-x64-gnu.tar.gz"
        );
    }

    #[test]
    fn archive_names_normalize_legacy_rust_triples() {
        let path = archive_path(
            Path::new("/out"),
            "bookclerk-plugin-source-audible",
            "0.1.0",
            "x86_64-unknown-linux-gnu",
        );
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.ends_with("linux-x64-gnu.tar.gz"));

        let win = archive_path(
            Path::new("/out"),
            "bookclerk-plugin-source-audible",
            "0.1.0",
            "x86_64-pc-windows-msvc",
        );
        assert!(win
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("windows-x64.zip"));
    }

    #[test]
    fn maps_host_triple_to_bookclerk_target() {
        assert_eq!(
            normalize_bookclerk_target("x86_64-unknown-linux-gnu"),
            Some("linux-x64-gnu")
        );
        assert_eq!(
            normalize_bookclerk_target("linux-x64-gnu"),
            Some("linux-x64-gnu")
        );
        assert!(uses_zip_archive("windows-x64"));
        assert!(!uses_zip_archive("linux-x64-gnu"));
    }
}
