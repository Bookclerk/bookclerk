//! Secure plugin installer state machine.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;

use crate::coordinate::{PackageCoordinate, RegistrySource};
use crate::error::{CatalogError, Result};
use crate::extract::{extract_archive, safe_join, sha256_file, write_file};
use crate::kind::RuntimeIdentity;
use crate::manifest::{parse_sha256_hex, BookclerkPackageManifest, PROTOCOL_JSONRPC_STDIO_V1};
use crate::receipt::InstallReceipt;
use crate::target::{host_bookclerk_target, select_target, ArchiveFormat};
use crate::trust::TrustPolicy;

/// Download / install limits.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
pub const MAX_REDIRECTS: u32 = 5;
pub const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

/// Options for [`Installer::install`].
#[derive(Debug, Clone)]
pub struct InstallOptions {
    pub plugins_root: PathBuf,
    pub target: Option<String>,
    pub dry_run: bool,
    pub replace: bool,
    pub offline: bool,
    pub trust: TrustPolicy,
    /// Skip health spawn (caller runs health separately).
    pub skip_health: bool,
    pub approve_capabilities: bool,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            plugins_root: PathBuf::from("plugins"),
            target: None,
            dry_run: false,
            replace: false,
            offline: false,
            trust: TrustPolicy::default(),
            skip_health: true,
            approve_capabilities: false,
        }
    }
}

/// Result of a successful (or dry-run) install.
#[derive(Debug, Clone)]
pub struct InstallOutcome {
    pub plugin_root: PathBuf,
    pub receipt: InstallReceipt,
    pub dry_run: bool,
    /// Previous install kept aside until [`Installer::commit`] (or restored by
    /// [`Installer::rollback`]). Present only for replace installs.
    pub previous: Option<PathBuf>,
}

/// Installer: resolve → download → verify → extract → activate.
pub struct Installer;

impl Installer {
    /// Install from an already-validated package manifest (fixture / adapter output).
    pub fn install_from_manifest(
        manifest: &BookclerkPackageManifest,
        coordinate: &PackageCoordinate,
        opts: &InstallOptions,
    ) -> Result<InstallOutcome> {
        manifest.validate_for_install()?;
        let artifact = select_target(
            &manifest.artifacts,
            |a| a.target.as_str(),
            opts.target.as_deref(),
        )?;
        let target = artifact.bookclerk_target();
        let runtime = manifest.runtime();
        validate_plugin_id(&runtime.id)?;

        let dest = safe_join(&opts.plugins_root, Path::new(&runtime.id))?;
        if dest.exists() {
            if let Ok(existing) = InstallReceipt::load(&dest) {
                let conflict = existing.runtime != runtime
                    || existing.coordinate.source.kind_name() != coordinate.source.kind_name()
                    || existing.coordinate.name != coordinate.name;
                if conflict && !opts.replace {
                    return Err(CatalogError::message(format!(
                        "runtime id `{}` already installed from {}; pass --replace",
                        runtime.id, existing.coordinate
                    )));
                }
                // Capability widening requires explicit approval even on --replace /
                // updates (replace must not bypass --approve-capabilities).
                if existing.requested_sandbox.network != manifest.sandbox.network
                    && !opts.approve_capabilities
                {
                    return Err(CatalogError::message(format!(
                        "update requests network `{}` (was `{}`); pass --approve-capabilities",
                        manifest.sandbox.network, existing.requested_sandbox.network
                    )));
                }
            } else if !opts.replace && dest.join("plugin.toml").exists() {
                return Err(CatalogError::message(format!(
                    "plugin `{}` already exists without receipt; pass --replace",
                    runtime.id
                )));
            }
        }

        opts.trust.check_unsigned_allowed()?;

        let expected = parse_sha256_hex(&artifact.archive_sha256)?;
        let _ = expected;

        if opts.dry_run {
            let receipt = build_receipt(manifest, coordinate, artifact, &target, None, &opts.trust);
            return Ok(InstallOutcome {
                plugin_root: dest,
                receipt,
                dry_run: true,
                previous: None,
            });
        }

        let staging_parent = opts.plugins_root.join(".staging");
        fs::create_dir_all(&staging_parent)?;
        let staging = staging_parent.join(format!("{}.{}", runtime.id, std::process::id()));
        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        fs::create_dir_all(&staging)?;

        let archive_path = staging.join("download.archive");
        download_to(&artifact.url, &archive_path, opts.offline)?;

        let actual = sha256_file(&archive_path)?;
        if !actual.eq_ignore_ascii_case(&artifact.archive_sha256) {
            let _ = fs::remove_dir_all(&staging);
            return Err(CatalogError::message(format!(
                "archive digest mismatch: expected {}, got {actual}",
                artifact.archive_sha256
            )));
        }

        let extract_root = staging.join("root");
        fs::create_dir_all(&extract_root)?;
        let format = if artifact.url.ends_with(".zip") || target.starts_with("windows-") {
            ArchiveFormat::Zip
        } else {
            ArchiveFormat::TarGz
        };
        extract_archive(&archive_path, format, &extract_root)?;

        let plugin_src = if artifact.archive_root == "." || artifact.archive_root.is_empty() {
            extract_root.clone()
        } else {
            safe_join(&extract_root, Path::new(&artifact.archive_root))?
        };
        let plugin_toml = plugin_src.join("plugin.toml");
        if !plugin_toml.is_file() {
            let _ = fs::remove_dir_all(&staging);
            return Err(CatalogError::message(
                "archive missing plugin.toml after extract",
            ));
        }
        let exe = safe_join(&plugin_src, Path::new(&artifact.executable))?;
        if !exe.is_file() {
            let _ = fs::remove_dir_all(&staging);
            return Err(CatalogError::message(format!(
                "archive missing executable {}",
                artifact.executable
            )));
        }
        let exe_digest = sha256_file(&exe)?;
        if let Some(expected_exe) = &artifact.executable_sha256 {
            if !exe_digest.eq_ignore_ascii_case(expected_exe) {
                let _ = fs::remove_dir_all(&staging);
                return Err(CatalogError::message("executable digest mismatch"));
            }
        }

        // Validate plugin.toml binds id/kind/sandbox/command to the package manifest.
        let toml_text = fs::read_to_string(&plugin_toml)?;
        validate_plugin_toml(
            &toml_text,
            &runtime,
            &manifest.sandbox.network,
            &artifact.executable,
        )?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&exe)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&exe, perms)?;
        }

        // Preserve per-plugin state across replace so updates do not wipe data/tmp.
        let state_hold = staging.join("preserved-state");
        if dest.exists() {
            peel_plugin_state(&dest, &state_hold)?;
        }

        let backup = if dest.exists() {
            let bak = staging_parent.join(format!("{}.backup", runtime.id));
            if bak.exists() {
                fs::remove_dir_all(&bak)?;
            }
            fs::rename(&dest, &bak)?;
            Some(bak)
        } else {
            None
        };

        // Move extracted plugin tree into place (contents of plugin_src → dest).
        if let Err(e) = copy_dir_all(&plugin_src, &dest) {
            let _ = fs::remove_dir_all(&dest);
            if let Some(bak) = &backup {
                let _ = fs::rename(bak, &dest);
            }
            let _ = restore_plugin_state(&state_hold, &dest);
            let _ = fs::remove_dir_all(&staging);
            return Err(CatalogError::message(format!(
                "activate install failed: {e}"
            )));
        }

        if let Err(e) = restore_plugin_state(&state_hold, &dest) {
            let _ = fs::remove_dir_all(&dest);
            if let Some(bak) = &backup {
                let _ = fs::rename(bak, &dest);
            }
            let _ = restore_plugin_state(&state_hold, &dest);
            let _ = fs::remove_dir_all(&staging);
            return Err(CatalogError::message(format!(
                "restore plugin state failed: {e}"
            )));
        }

        let receipt = build_receipt(
            manifest,
            coordinate,
            artifact,
            &target,
            Some(exe_digest),
            &opts.trust,
        );
        if let Err(e) = receipt.store(&dest) {
            let _ = fs::remove_dir_all(&dest);
            if let Some(bak) = &backup {
                let _ = fs::rename(bak, &dest);
            }
            let _ = restore_plugin_state(&state_hold, &dest);
            let _ = fs::remove_dir_all(&staging);
            return Err(e);
        }

        // Drop staging (download/extract/state hold). Leave `previous` backup for
        // commit/rollback after the caller's health check.
        let _ = fs::remove_dir_all(&staging);

        Ok(InstallOutcome {
            plugin_root: dest,
            receipt,
            dry_run: false,
            previous: backup,
        })
    }

    /// Discard a replace backup after a successful health check (or when none).
    pub fn commit(outcome: &InstallOutcome) -> Result<()> {
        if let Some(bak) = &outcome.previous {
            if bak.exists() {
                remove_dir_retry(bak)?;
            }
        }
        Ok(())
    }

    /// Restore the previous install after a failed health check.
    pub fn rollback(outcome: &InstallOutcome) -> Result<()> {
        let Some(bak) = &outcome.previous else {
            return Ok(());
        };
        if outcome.plugin_root.exists() {
            // Peel state from the failed new install so it survives on the old tree.
            let hold = bak
                .parent()
                .unwrap_or(Path::new("."))
                .join(format!("{}.rollback-state", outcome.receipt.runtime.id));
            let _ = peel_plugin_state(&outcome.plugin_root, &hold);
            let _ = fs::remove_dir_all(&outcome.plugin_root);
            fs::rename(bak, &outcome.plugin_root)?;
            let _ = restore_plugin_state(&hold, &outcome.plugin_root);
            let _ = fs::remove_dir_all(&hold);
        } else if bak.exists() {
            fs::rename(bak, &outcome.plugin_root)?;
        }
        Ok(())
    }

    /// Install from a local archive path using an explicit manifest.
    pub fn install_local_archive(
        archive: &Path,
        manifest: &BookclerkPackageManifest,
        opts: &InstallOptions,
    ) -> Result<InstallOutcome> {
        let coordinate = PackageCoordinate {
            source: RegistrySource::LocalArchive,
            name: archive.display().to_string(),
            version: manifest
                .coordinate
                .as_ref()
                .map(|c| c.version.clone())
                .unwrap_or_else(|| "0.0.0".into()),
        };
        // Rewrite artifact URL to the local file for the selected target.
        let mut m = manifest.clone();
        let host = opts
            .target
            .clone()
            .unwrap_or_else(|| host_bookclerk_target().to_string());
        for art in &mut m.artifacts {
            if crate::target::normalize_target(&art.target).unwrap_or(art.target.as_str())
                == crate::target::normalize_target(&host).unwrap_or(host.as_str())
            {
                art.url = format!("file://{}", archive.display());
                // Recompute digest from local file.
                art.archive_sha256 = sha256_file(archive)?;
            }
        }
        Self::install_from_manifest(&m, &coordinate, opts)
    }

    /// Remove an installed plugin directory; optionally purge data/tmp state.
    pub fn remove(plugins_root: &Path, id: &str, purge_state: bool) -> Result<()> {
        validate_plugin_id(id)?;
        let dest = safe_join(plugins_root, Path::new(id))?;
        if !dest.exists() {
            return Err(CatalogError::message(format!(
                "plugin `{id}` is not installed under {}",
                plugins_root.display()
            )));
        }
        // Keep data/tmp unless purge — move them aside if !purge and they live under dest.
        let data = dest.join("data");
        let tmp = dest.join("tmp");
        let state_backup = plugins_root.join(format!(".state-keep-{id}"));
        if !purge_state && (data.exists() || tmp.exists()) {
            fs::create_dir_all(&state_backup)?;
            if data.exists() {
                let _ = fs::rename(&data, state_backup.join("data"));
            }
            if tmp.exists() {
                let _ = fs::rename(&tmp, state_backup.join("tmp"));
            }
        }
        // Retry-friendly remove (Windows file locks).
        remove_dir_retry(&dest)?;
        if purge_state {
            let _ = fs::remove_dir_all(plugins_root.join(id).join("data"));
            let _ = fs::remove_dir_all(state_backup);
        } else if state_backup.exists() {
            // Leave state in .state-keep-<id> for possible reinstall.
        }
        Ok(())
    }
}

fn build_receipt(
    manifest: &BookclerkPackageManifest,
    coordinate: &PackageCoordinate,
    artifact: &crate::manifest::ArtifactTarget,
    target: &str,
    exe_digest: Option<String>,
    trust: &TrustPolicy,
) -> InstallReceipt {
    let registry_url = match &coordinate.source {
        RegistrySource::Cargo { registry_url } => Some(registry_url.clone()),
        RegistrySource::Npm { registry_url } => Some(registry_url.clone()),
        RegistrySource::Pypi { simple_url } => Some(simple_url.clone()),
        RegistrySource::Static { index_url } => Some(index_url.clone()),
        RegistrySource::LocalArchive => None,
    };
    InstallReceipt {
        schema_version: InstallReceipt::SCHEMA_VERSION,
        coordinate: coordinate.clone(),
        version: coordinate.version.clone(),
        registry_url,
        artifact_url: artifact.url.clone(),
        target: target.to_string(),
        archive_sha256: artifact.archive_sha256.clone(),
        executable_sha256: exe_digest.or_else(|| artifact.executable_sha256.clone()),
        protocol: manifest
            .protocol
            .clone()
            .if_empty_then(PROTOCOL_JSONRPC_STDIO_V1),
        api_version: manifest.api_version,
        runtime: manifest.runtime(),
        requested_sandbox: manifest.sandbox.clone(),
        approved_network: manifest.sandbox.network.clone(),
        installed_at: Utc::now(),
        update_constraint: None,
        publisher_key_id: manifest.publisher.as_ref().and_then(|p| p.key_id.clone()),
        allow_unsigned: trust.allow_unsigned,
    }
}

trait IfEmpty {
    fn if_empty_then(self, default: &str) -> String;
}
impl IfEmpty for String {
    fn if_empty_then(self, default: &str) -> String {
        if self.is_empty() {
            default.into()
        } else {
            self
        }
    }
}

/// Reject absolute / traversing plugin ids before joining under `plugins_root`.
fn validate_plugin_id(id: &str) -> Result<()> {
    if id.is_empty() {
        return Err(CatalogError::message("plugin id must not be empty"));
    }
    if id.contains('\0') {
        return Err(CatalogError::message("plugin id must not contain NUL"));
    }
    let path = Path::new(id);
    if path.is_absolute() {
        return Err(CatalogError::message(format!(
            "refusing absolute plugin id: {id}"
        )));
    }
    use std::path::Component;
    for comp in path.components() {
        match comp {
            Component::Normal(c) => {
                if c.is_empty() {
                    return Err(CatalogError::message(format!(
                        "refusing empty component in plugin id: {id}"
                    )));
                }
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(CatalogError::message(format!(
                    "refusing path-escaping plugin id: {id}"
                )));
            }
        }
    }
    Ok(())
}

fn validate_plugin_toml(
    text: &str,
    runtime: &RuntimeIdentity,
    expected_network: &str,
    expected_exe: &str,
) -> Result<()> {
    let value: toml::Value = toml::from_str(text)?;
    let id = value
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CatalogError::message("plugin.toml missing id"))?;
    let kind = value
        .get("kind")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CatalogError::message("plugin.toml missing kind"))?;
    if id != runtime.id {
        return Err(CatalogError::message(format!(
            "plugin.toml id `{id}` does not match package id `{}`",
            runtime.id
        )));
    }
    if kind != runtime.kind.as_str() {
        return Err(CatalogError::message(format!(
            "plugin.toml kind `{kind}` does not match package kind `{}`",
            runtime.kind
        )));
    }
    let network = value
        .get("sandbox")
        .and_then(|s| s.get("network"))
        .and_then(|v| v.as_str())
        .unwrap_or("outbound");
    let expected = if expected_network.is_empty() {
        "outbound"
    } else {
        expected_network
    };
    if network != expected {
        return Err(CatalogError::message(format!(
            "plugin.toml sandbox.network `{network}` does not match package sandbox `{expected}`"
        )));
    }
    let command = value
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CatalogError::message("plugin.toml missing command"))?;
    if !command_matches_executable(command, expected_exe) {
        return Err(CatalogError::message(format!(
            "plugin.toml command `{command}` does not match package executable `{expected_exe}`"
        )));
    }
    Ok(())
}

fn command_matches_executable(command: &str, executable: &str) -> bool {
    let cmd = Path::new(command.trim_start_matches("./"));
    let exe = Path::new(executable.trim_start_matches("./"));
    match (cmd.file_name(), exe.file_name()) {
        (Some(a), Some(b)) => a == b,
        _ => command == executable,
    }
}

fn peel_plugin_state(plugin_root: &Path, hold: &Path) -> Result<()> {
    let data = plugin_root.join("data");
    let tmp = plugin_root.join("tmp");
    if !data.exists() && !tmp.exists() {
        return Ok(());
    }
    fs::create_dir_all(hold)?;
    if data.exists() {
        let dest = hold.join("data");
        if dest.exists() {
            fs::remove_dir_all(&dest)?;
        }
        fs::rename(&data, &dest)?;
    }
    if tmp.exists() {
        let dest = hold.join("tmp");
        if dest.exists() {
            fs::remove_dir_all(&dest)?;
        }
        fs::rename(&tmp, &dest)?;
    }
    Ok(())
}

fn restore_plugin_state(hold: &Path, plugin_root: &Path) -> Result<()> {
    if !hold.exists() {
        return Ok(());
    }
    for name in ["data", "tmp"] {
        let src = hold.join(name);
        if !src.exists() {
            continue;
        }
        let dest = plugin_root.join(name);
        if dest.exists() {
            fs::remove_dir_all(&dest)?;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&src, &dest)?;
    }
    Ok(())
}

fn download_to(url: &str, dest: &Path, offline: bool) -> Result<()> {
    if let Some(path) = url.strip_prefix("file://") {
        fs::copy(path, dest)?;
        return Ok(());
    }
    if url.starts_with('/') || Path::new(url).exists() {
        fs::copy(url, dest)?;
        return Ok(());
    }
    if offline {
        return Err(CatalogError::message(
            "offline install requires a local file:// artifact url",
        ));
    }
    if !url.starts_with("https://")
        && !url.starts_with("http://127.0.0.1")
        && !url.starts_with("http://localhost")
    {
        return Err(CatalogError::message(
            "remote install requires https:// (or localhost http for fixtures)",
        ));
    }
    let _ = MAX_REDIRECTS;
    let _ = DOWNLOAD_TIMEOUT;
    let mut response = ureq::get(url)
        .header(
            "User-Agent",
            concat!("bookclerk/", env!("CARGO_PKG_VERSION"), " (plugin-install)"),
        )
        .call()
        .map_err(|e| CatalogError::message(format!("download failed: {e}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(CatalogError::message(format!(
            "download HTTP {status} for {url}"
        )));
    }
    let mut buf = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_DOWNLOAD_BYTES + 1)
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_DOWNLOAD_BYTES {
        return Err(CatalogError::message("download exceeds size limit"));
    }
    write_file(dest, &buf)?;
    Ok(())
}

fn copy_dir_all(src: &Path, dest: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dest)?;
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry?;
        let path = entry.path();
        let rel = path.strip_prefix(src).unwrap();
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = dest.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&out)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(path, &out)?;
        }
    }
    Ok(())
}

fn remove_dir_retry(path: &Path) -> Result<()> {
    let mut last = None;
    for _ in 0..5 {
        match fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    Err(CatalogError::message(format!(
        "failed to remove {}: {}",
        path.display(),
        last.map(|e| e.to_string()).unwrap_or_default()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::sha256_bytes;
    use crate::kind::PluginKind;
    use crate::manifest::ArtifactTarget;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use tar::Builder;

    fn make_echo_archive(dir: &Path) -> (PathBuf, String) {
        let archive = dir.join("echo.tar.gz");
        {
            let file = fs::File::create(&archive).unwrap();
            let enc = GzEncoder::new(file, Compression::default());
            let mut tar = Builder::new(enc);
            let toml =
                b"api_version = 1\nid = \"echo\"\nkind = \"integration\"\ncommand = \"./echo\"\n";
            let mut h = tar::Header::new_gnu();
            h.set_size(toml.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, "plugin.toml", &toml[..]).unwrap();
            let bin = b"#!/bin/sh\necho ok\n";
            let mut h2 = tar::Header::new_gnu();
            h2.set_size(bin.len() as u64);
            h2.set_mode(0o755);
            h2.set_cksum();
            tar.append_data(&mut h2, "echo", &bin[..]).unwrap();
            let enc = tar.into_inner().unwrap();
            enc.finish().unwrap();
        }
        let digest = sha256_file(&archive).unwrap();
        (archive, digest)
    }

    #[test]
    fn atomic_install_and_receipt() {
        let tmp = tempfile::tempdir().unwrap();
        let (archive, digest) = make_echo_archive(tmp.path());
        let target = host_bookclerk_target();
        let manifest = BookclerkPackageManifest {
            schema_version: 1,
            protocol: PROTOCOL_JSONRPC_STDIO_V1.into(),
            api_version: 1,
            api_version_max: None,
            min_bookclerk: None,
            kind: PluginKind::Integration,
            id: "echo".into(),
            display_name: Some("Echo".into()),
            description: None,
            coordinate: None,
            artifacts: vec![ArtifactTarget {
                target: target.into(),
                url: format!("file://{}", archive.display()),
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
        };
        let plugins = tmp.path().join("plugins");
        let opts = InstallOptions {
            plugins_root: plugins.clone(),
            trust: TrustPolicy {
                allow_unsigned: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let coord = PackageCoordinate {
            source: RegistrySource::LocalArchive,
            name: archive.display().to_string(),
            version: "1.0.0".into(),
        };
        let out = Installer::install_from_manifest(&manifest, &coord, &opts).unwrap();
        assert!(out.plugin_root.join("plugin.toml").is_file());
        Installer::commit(&out).unwrap();
        let receipt = InstallReceipt::load(&out.plugin_root).unwrap();
        assert_eq!(receipt.runtime.id, "echo");

        // Digest mismatch fails.
        let mut bad = manifest.clone();
        bad.artifacts[0].archive_sha256 = sha256_bytes(b"nope");
        // pad to 64 hex
        bad.artifacts[0].archive_sha256 = format!("{:0>64}", "ab");
        assert!(Installer::install_from_manifest(&bad, &coord, &opts).is_err());
    }

    #[test]
    fn rejects_escaping_plugin_id() {
        let err = validate_plugin_id("../evil").unwrap_err();
        assert!(err.to_string().contains("escaping"), "{err}");
        let err = validate_plugin_id("/abs").unwrap_err();
        assert!(err.to_string().contains("absolute"), "{err}");
    }

    #[test]
    fn toml_binds_sandbox_and_command() {
        let runtime = RuntimeIdentity::new(PluginKind::Integration, "echo");
        let ok = "api_version = 1\nid = \"echo\"\nkind = \"integration\"\ncommand = \"./echo\"\n[sandbox]\nnetwork = \"none\"\n";
        validate_plugin_toml(ok, &runtime, "none", "echo").unwrap();
        let bad_net = "api_version = 1\nid = \"echo\"\nkind = \"integration\"\ncommand = \"./echo\"\n[sandbox]\nnetwork = \"listen\"\n";
        assert!(validate_plugin_toml(bad_net, &runtime, "none", "echo").is_err());
        let bad_cmd =
            "api_version = 1\nid = \"echo\"\nkind = \"integration\"\ncommand = \"./other\"\n";
        assert!(validate_plugin_toml(bad_cmd, &runtime, "outbound", "echo").is_err());
    }
}
