//! Pin and fetch of the ONNX Runtime shared library.
//!
//! The model plugin (`crates/bookclerk-plugins/platform/ai-embedder`) runs
//! its graphs with Microsoft's C++ ONNX Runtime, loaded as a shared
//! library that sits beside the plugin's executable. This crate names the
//! one release every build uses (`onnxruntime-pin.json`) and fetches it:
//! `cargo ensure-onnxruntime`, the staging and packaging commands and the
//! plugin's tests all call [`ensure_onnxruntime`].
//!
//! The library is taken from the release's wheel on PyPI, which carries the
//! same shared library as Microsoft's release archives. The wheel's
//! SHA-256 is checked before it is opened, and the library's own SHA-256
//! after it is extracted, so a cached copy is recognised by content.
//!
//! No release exists for Intel macOS; [`host_asset`] is `None` there and
//! the plugin is left out of that build.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bookclerk_tls::ureq;
use sha2::{Digest, Sha256};

/// The pin file, embedded at build time.
const PIN_JSON: &str = include_str!("../onnxruntime-pin.json");

/// Name of the shared library beside the plugin executable on this
/// platform. The model plugin looks for exactly this name.
pub const LIBRARY_FILE_NAME: &str = if cfg!(target_os = "windows") {
    "onnxruntime.dll"
} else if cfg!(target_os = "macos") {
    "libonnxruntime.dylib"
} else {
    "libonnxruntime.so"
};

/// How many times a download is tried.
const DOWNLOAD_ATTEMPTS: u32 = 4;

/// Time one download attempt may take.
const DOWNLOAD_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(300);

/// Most bytes of a wheel that are read (the largest pinned one is 28 MB).
const MAX_WHEEL_BYTES: u64 = 128 * 1024 * 1024;

/// The pinned release: one version and one wheel per platform.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pin {
    /// ONNX Runtime release (`1.32.0`).
    pub version: String,
    /// The wheel for each platform, keyed `<os>-<arch>` in the words of
    /// [`std::env::consts`] (`linux-x86_64`, `macos-aarch64`).
    pub assets: std::collections::BTreeMap<String, Asset>,
}

/// Where one platform's library comes from and what it must be.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    /// HTTPS address of the wheel.
    pub wheel_url: String,
    /// SHA-256 of the wheel, lowercase hexadecimal.
    pub wheel_sha256_hex: String,
    /// Path of the shared library inside the wheel.
    pub member: String,
    /// Length of the shared library in bytes.
    pub library_bytes: u64,
    /// SHA-256 of the shared library, lowercase hexadecimal.
    pub library_sha256_hex: String,
}

/// Reads the embedded pin.
///
/// # Errors
///
/// When `onnxruntime-pin.json` is not the document [`Pin`] describes; a
/// test keeps that from reaching a build.
pub fn pin() -> Result<Pin> {
    serde_json::from_str(PIN_JSON).context("parse onnxruntime-pin.json")
}

/// The pinned wheel for the platform this build targets.
///
/// # Returns
///
/// `None` when ONNX Runtime publishes no library for the platform.
///
/// # Errors
///
/// When the embedded pin cannot be read.
pub fn host_asset() -> Result<Option<Asset>> {
    let key = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    Ok(pin()?.assets.remove(&key))
}

/// Lowercase hexadecimal SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Whether `path` is a regular file whose content is the pinned library.
fn is_pinned_library(path: &Path, asset: &Asset) -> bool {
    let regular = fs::symlink_metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.len() == asset.library_bytes);
    regular && fs::read(path).is_ok_and(|bytes| sha256_hex(&bytes) == asset.library_sha256_hex)
}

/// Takes the shared library out of a wheel.
///
/// # Arguments
///
/// * `wheel` - The wheel's bytes, already checked against the pin.
/// * `asset` - The pin entry naming the member and what it must be.
///
/// # Errors
///
/// When the wheel is not a zip archive, the member is missing, or the
/// member's length or SHA-256 differs from the pin.
pub fn extract_library(wheel: &[u8], asset: &Asset) -> Result<Vec<u8>> {
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(wheel)).context("open the wheel as a zip")?;
    let member = archive
        .by_name(&asset.member)
        .with_context(|| format!("find {} in the wheel", asset.member))?;
    let mut library = Vec::new();
    // One byte past the pinned length is enough to notice a longer member.
    member
        .take(asset.library_bytes.saturating_add(1))
        .read_to_end(&mut library)
        .with_context(|| format!("read {} from the wheel", asset.member))?;
    let same_length = u64::try_from(library.len()).is_ok_and(|len| len == asset.library_bytes);
    if !same_length || sha256_hex(&library) != asset.library_sha256_hex {
        bail!("{} in the wheel does not match the pin", asset.member);
    }
    Ok(library)
}

/// Downloads `url` once.
fn download_once(url: &str) -> Result<Vec<u8>> {
    let request = ureq::get(url)
        .config()
        .timeout_global(Some(DOWNLOAD_ATTEMPT_TIMEOUT))
        .build();
    let mut response = request.call().with_context(|| format!("GET {url}"))?;
    if !response.status().is_success() {
        bail!("GET {url} returned {}", response.status());
    }
    let mut body = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_WHEEL_BYTES)
        .read_to_end(&mut body)
        .context("read download body")?;
    Ok(body)
}

/// Downloads `url`, trying again after a failure with a doubling pause.
fn download(url: &str) -> Result<Vec<u8>> {
    let mut pause = Duration::from_secs(1);
    let mut attempt = 1;
    loop {
        match download_once(url) {
            Ok(body) => return Ok(body),
            Err(err) if attempt >= DOWNLOAD_ATTEMPTS => return Err(err),
            Err(err) => {
                eprintln!("bookclerk-onnxruntime: attempt {attempt} failed: {err:#}");
                std::thread::sleep(pause);
                pause = pause.saturating_mul(2);
                attempt += 1;
            }
        }
    }
}

/// Writes `library` to `dest` through a sibling temporary file, so a
/// reader never sees half a library.
fn install(library: &[u8], dest: &Path) -> Result<()> {
    let dir = dest
        .parent()
        .with_context(|| format!("{} has no parent", dest.display()))?;
    let staging = dir.join(format!(
        ".{LIBRARY_FILE_NAME}.tmp-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos())
    ));
    let written = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)
            .with_context(|| format!("create {}", staging.display()))?;
        file.write_all(library)
            .with_context(|| format!("write {}", staging.display()))?;
        file.sync_all().ok();
        drop(file);
        // Windows cannot rename over an existing file.
        if cfg!(windows) && fs::symlink_metadata(dest).is_ok() {
            fs::remove_file(dest).with_context(|| format!("replace {}", dest.display()))?;
        }
        fs::rename(&staging, dest)
            .with_context(|| format!("move {} to {}", staging.display(), dest.display()))
    })();
    if written.is_err() {
        let _ = fs::remove_file(&staging);
    }
    written
}

/// Makes sure the pinned ONNX Runtime library is in `dir`, downloading it
/// when it is missing or is another release.
///
/// # Arguments
///
/// * `dir` - Directory to hold [`LIBRARY_FILE_NAME`]; created when missing.
///
/// # Returns
///
/// The library's path, or `None` on a platform ONNX Runtime has no release
/// for.
///
/// # Errors
///
/// When the download fails, a digest differs from the pin, or the file
/// cannot be written. A symbolic link in the library's place is refused.
pub fn ensure_onnxruntime(dir: &Path) -> Result<Option<PathBuf>> {
    let Some(asset) = host_asset()? else {
        return Ok(None);
    };
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let dest = dir.join(LIBRARY_FILE_NAME);
    if fs::symlink_metadata(&dest).is_ok_and(|meta| meta.file_type().is_symlink()) {
        bail!("refusing a symbolic link at {}", dest.display());
    }
    if is_pinned_library(&dest, &asset) {
        return Ok(Some(dest));
    }
    eprintln!("bookclerk-onnxruntime: fetching {}", asset.wheel_url);
    let wheel = download(&asset.wheel_url)?;
    if sha256_hex(&wheel) != asset.wheel_sha256_hex {
        bail!("the wheel from {} does not match the pin", asset.wheel_url);
    }
    let library = extract_library(&wheel, &asset)?;
    install(&library, &dest)?;
    Ok(Some(dest))
}

/// Makes sure the pinned library is in the build tree's shared cache,
/// `<target>/onnxruntime-<version>/`, which the staging commands and the
/// model plugin's tests all read.
///
/// # Arguments
///
/// * `target` - Cargo's target directory.
///
/// # Returns
///
/// The library's path, or `None` on a platform ONNX Runtime has no release
/// for.
///
/// # Errors
///
/// As [`ensure_onnxruntime`].
pub fn ensure_in_target(target: &Path) -> Result<Option<PathBuf>> {
    let version = pin()?.version;
    ensure_onnxruntime(&target.join(format!("onnxruntime-{version}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A zip archive with the given members.
    fn zip_of(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, content) in members {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(content).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    /// A pin entry for a library with the given content.
    fn asset_for(member: &str, library: &[u8]) -> Asset {
        Asset {
            wheel_url: "https://example.invalid/wheel.whl".into(),
            wheel_sha256_hex: String::new(),
            member: member.into(),
            library_bytes: library.len() as u64,
            library_sha256_hex: sha256_hex(library),
        }
    }

    #[test]
    fn the_embedded_pin_is_well_formed() {
        let pin = pin().unwrap();
        assert!(pin.version.split('.').count() == 3, "{}", pin.version);
        for key in [
            "linux-x86_64",
            "linux-aarch64",
            "macos-aarch64",
            "windows-x86_64",
        ] {
            let asset = pin.assets.get(key).unwrap_or_else(|| panic!("{key}"));
            assert!(asset
                .wheel_url
                .starts_with("https://files.pythonhosted.org/packages/"));
            assert!(asset
                .wheel_url
                .contains(&format!("onnxruntime-{}-", pin.version)));
            for digest in [&asset.wheel_sha256_hex, &asset.library_sha256_hex] {
                assert_eq!(digest.len(), 64);
                assert!(digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
            }
            assert!(asset.member.starts_with("onnxruntime/capi/"));
            assert!(asset.library_bytes > 1_000_000);
        }
        assert_eq!(pin.assets.len(), 4);
    }

    #[test]
    fn the_library_is_extracted_only_when_it_matches_the_pin() {
        let wheel = zip_of(&[("other.txt", b"x"), ("pkg/lib.so", b"library bytes")]);
        let asset = asset_for("pkg/lib.so", b"library bytes");
        assert_eq!(extract_library(&wheel, &asset).unwrap(), b"library bytes");

        let missing = asset_for("pkg/absent.so", b"library bytes");
        assert!(extract_library(&wheel, &missing).is_err());
        let other_content = asset_for("pkg/lib.so", b"library bytez");
        assert!(extract_library(&wheel, &other_content).is_err());
        let mut longer = asset_for("pkg/lib.so", b"library bytes");
        longer.library_bytes -= 1;
        assert!(extract_library(&wheel, &longer).is_err());
        assert!(extract_library(b"not a zip", &asset).is_err());
    }

    #[test]
    fn a_library_on_disk_is_recognised_by_content() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join(LIBRARY_FILE_NAME);
        let asset = asset_for("m", b"pinned");
        assert!(!is_pinned_library(&dest, &asset));
        install(b"pinned", &dest).unwrap();
        assert!(is_pinned_library(&dest, &asset));
        install(b"pinnee", &dest).unwrap();
        assert!(!is_pinned_library(&dest, &asset));
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_failed_download_is_an_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n");
        });
        let err = download_once(&format!("http://{addr}/wheel.whl")).unwrap_err();
        assert!(format!("{err:#}").contains("404"), "{err:#}");
    }

    #[test]
    fn a_download_returns_the_body() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nwheel");
        });
        assert_eq!(
            download(&format!("http://{addr}/wheel.whl")).unwrap(),
            b"wheel"
        );
    }
}
