//! Download / refresh the pinned Cloudflare `workerd` binary.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

use crate::pin::{
    binary_name, download_url, host_asset, WORKERD_RELEASE_TAG, WORKERD_VERSION_STAMP,
};

/// Ensure `dir/workerd` matches [`WORKERD_RELEASE_TAG`], downloading if needed.
///
/// Returns the path to the executable. Honors `BOOKCLERK_WORKERD_BIN` when set:
/// if that path exists and its version stamp (sibling `workerd.version`) or
/// `--version` output matches the pin, it is returned; otherwise ensure still
/// installs into `dir` (override path is not overwritten).
///
/// # Arguments
///
/// * `dir` - Directory path for this operation.
///
/// # Returns
///
/// On success, the inner `PathBuf` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn ensure_workerd(dir: &Path) -> Result<PathBuf> {
    if let Ok(override_bin) = std::env::var("BOOKCLERK_WORKERD_BIN") {
        let path = PathBuf::from(override_bin);
        if path.is_file() && is_current(&path)? {
            return Ok(path);
        }
        // Stale/missing override: fall through to managed install in `dir`.
    }

    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let dest = dir.join(binary_name());
    if dest.is_file() && is_current(&dest)? {
        return Ok(dest);
    }

    let asset = host_asset().with_context(|| {
        format!(
            "no pinned workerd asset for {}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;

    let url = download_url(asset.artifact);
    eprintln!("bookclerk-workerd: fetching {url}");
    let compressed = download(&url).with_context(|| format!("download {url}"))?;
    verify_sha256(&compressed, asset.sha256_hex)?;

    let mut decoder = GzDecoder::new(compressed.as_slice());
    let mut binary = Vec::new();
    decoder
        .read_to_end(&mut binary)
        .context("gunzip workerd payload")?;

    let tmp = dir.join(format!("{}.tmp", binary_name()));
    {
        let mut f = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(&binary)
            .with_context(|| format!("write {}", tmp.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&tmp, perms)?;
    }
    fs::rename(&tmp, &dest).with_context(|| {
        format!(
            "install workerd → {} (replace {})",
            dest.display(),
            tmp.display()
        )
    })?;

    let stamp = dir.join(WORKERD_VERSION_STAMP);
    fs::write(&stamp, format!("{WORKERD_RELEASE_TAG}\n"))
        .with_context(|| format!("write {}", stamp.display()))?;

    eprintln!(
        "bookclerk-workerd: installed {WORKERD_RELEASE_TAG} → {}",
        dest.display()
    );
    Ok(dest)
}

/// Preferred install directory: beside this process, else `dir` argument from callers.
///
/// # Arguments
///
/// * `dir` - Directory path for this operation.
///
/// # Returns
///
/// `PathBuf` result.
#[must_use]
pub fn workerd_bin_path(dir: &Path) -> PathBuf {
    if let Ok(override_bin) = std::env::var("BOOKCLERK_WORKERD_BIN") {
        return PathBuf::from(override_bin);
    }
    dir.join(binary_name())
}

fn is_current(bin: &Path) -> Result<bool> {
    if let Some(dir) = bin.parent() {
        let stamp = dir.join(WORKERD_VERSION_STAMP);
        if stamp.is_file() {
            let text = fs::read_to_string(&stamp).unwrap_or_default();
            if text.trim() == WORKERD_RELEASE_TAG {
                return Ok(true);
            }
        }
    }
    // Fallback: ask the binary (best-effort).
    let output = std::process::Command::new(bin)
        .arg("--version")
        .output()
        .with_context(|| format!("run {} --version", bin.display()))?;
    if !output.status.success() {
        return Ok(false);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");
    // Tags look like v1.20260810.1; binaries often print without the leading v.
    let pin = WORKERD_RELEASE_TAG.trim_start_matches('v');
    Ok(combined.contains(WORKERD_RELEASE_TAG) || combined.contains(pin))
}

fn download(url: &str) -> Result<Vec<u8>> {
    let mut response = ureq::get(url)
        .call()
        .with_context(|| format!("GET {url}"))?;
    if !response.status().is_success() {
        bail!("GET {url} returned {}", response.status());
    }
    let mut body = Vec::new();
    response
        .body_mut()
        .as_reader()
        .read_to_end(&mut body)
        .context("read download body")?;
    Ok(body)
}

fn verify_sha256(bytes: &[u8], expected_hex: &str) -> Result<()> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let got = hex::encode(hasher.finalize());
    if got != expected_hex {
        bail!("workerd download sha256 mismatch: got {got}, expected {expected_hex}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_asset_defined_for_ci_linux() {
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            assert!(host_asset().is_some());
        }
    }
}
