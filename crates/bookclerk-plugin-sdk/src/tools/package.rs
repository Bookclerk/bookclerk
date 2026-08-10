//! `bookclerk-plugin package`

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use bookclerk_plugin_manifest::{parse, PluginRuntimeKind};
use flate2::write::GzEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};

use crate::error::{Result, SdkError};

/// Package a plugin directory into `out_dir`, returning the archive path.
pub fn package_plugin(plugin_dir: &Path, out_dir: &Path) -> Result<PathBuf> {
    let toml_path = plugin_dir.join("plugin.toml");
    let text = std::fs::read_to_string(&toml_path)
        .map_err(|e| SdkError::message(format!("read {}: {e}", toml_path.display())))?;
    let manifest = parse(&text).map_err(|e| SdkError::message(e.to_string()))?;
    let version = manifest.version.clone().unwrap_or_else(|| "0.0.0".into());
    let id = &manifest.id;
    std::fs::create_dir_all(out_dir).map_err(SdkError::from)?;

    let staging = out_dir.join(format!(".staging-{id}"));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(SdkError::from)?;
    std::fs::copy(&toml_path, staging.join("plugin.toml")).map_err(SdkError::from)?;

    if let Some(logo) = manifest.logo.as_deref() {
        if let bookclerk_plugin_manifest::LogoKind::EmbeddedPath(rel) =
            bookclerk_plugin_manifest::validate_logo(logo)
                .map_err(|e| SdkError::message(e.to_string()))?
        {
            let src = plugin_dir.join(&rel);
            if !src.is_file() {
                return Err(SdkError::message(format!(
                    "embedded logo missing for package: {}",
                    src.display()
                )));
            }
            let dest = staging.join(&rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(SdkError::from)?;
            }
            std::fs::copy(&src, &dest).map_err(SdkError::from)?;
        }
    }

    let archive_stem = match manifest.runtime {
        PluginRuntimeKind::Native => {
            let cmd = manifest
                .command
                .as_ref()
                .ok_or_else(|| SdkError::message("native plugin missing command"))?;
            let src = if cmd.is_absolute() {
                cmd.clone()
            } else {
                plugin_dir.join(cmd)
            };
            if !src.is_file() {
                return Err(SdkError::message(format!(
                    "native binary not found for package: {}",
                    src.display()
                )));
            }
            let bin_name = src
                .file_name()
                .ok_or_else(|| SdkError::message("binary name"))?
                .to_os_string();
            let dest = staging.join(&bin_name);
            std::fs::copy(&src, &dest).map_err(SdkError::from)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&dest)?.permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&dest, perms)?;
            }
            let triple = host_bookclerk_target();
            format!("bookclerk-plugin-{id}-{version}-{triple}")
        }
        PluginRuntimeKind::Workerd => {
            let w = manifest
                .workerd
                .as_ref()
                .ok_or_else(|| SdkError::message("workerd config missing"))?;
            let modules_src = plugin_dir.join(&w.modules_dir);
            let modules_dst = staging.join(&w.modules_dir);
            copy_dir_recursive(&modules_src, &modules_dst)?;
            // BookclerkPlugin is imported from `@bookclerk/plugin-sdk/workerd`;
            // `bookclerk-workerd` injects that module at runtime.
            format!("bookclerk-plugin-{id}-{version}-workerd")
        }
    };

    let archive_name = format!("{archive_stem}.tar.gz");
    let archive_path = out_dir.join(&archive_name);
    write_tar_gz(&staging, &archive_path)?;
    let _ = std::fs::remove_dir_all(&staging);

    let sums = out_dir.join("SHA256SUMS");
    let digest = sha256_file(&archive_path)?;
    let line = format!("{digest}  {archive_name}\n");
    // Append or replace single-line sums for this archive.
    let mut body = String::new();
    if sums.is_file() {
        body = std::fs::read_to_string(&sums).unwrap_or_default();
        body = body
            .lines()
            .filter(|l| !l.ends_with(&archive_name))
            .collect::<Vec<_>>()
            .join("\n");
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
    }
    body.push_str(&line);
    std::fs::write(&sums, body).map_err(SdkError::from)?;

    Ok(archive_path)
}

fn host_bookclerk_target() -> String {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    match (os, arch) {
        ("linux", "x86_64") => "linux-x64-gnu".into(),
        ("linux", "aarch64") => "linux-arm64".into(),
        ("macos", "aarch64") => "macos-arm64".into(),
        ("macos", "x86_64") => "macos-x64".into(),
        ("windows", "x86_64") => "windows-x64".into(),
        _ => format!("{os}-{arch}"),
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).map_err(SdkError::from)?;
    for entry in std::fs::read_dir(src).map_err(SdkError::from)? {
        let entry = entry.map_err(SdkError::from)?;
        let ty = entry.file_type().map_err(SdkError::from)?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), to).map_err(SdkError::from)?;
        }
    }
    Ok(())
}

fn write_tar_gz(staging: &Path, archive_path: &Path) -> Result<()> {
    let file = File::create(archive_path).map_err(SdkError::from)?;
    let enc = GzEncoder::new(file, Compression::default());
    let mut builder = tar::Builder::new(enc);
    builder
        .append_dir_all(".", staging)
        .map_err(|e| SdkError::message(format!("tar: {e}")))?;
    let enc = builder
        .into_inner()
        .map_err(|e| SdkError::message(format!("tar finish: {e}")))?;
    enc.finish()
        .map_err(|e| SdkError::message(format!("gzip finish: {e}")))?;
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut f = File::open(path).map_err(SdkError::from)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = f.read(&mut buf).map_err(SdkError::from)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}
