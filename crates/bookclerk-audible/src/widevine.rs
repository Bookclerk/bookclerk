//! Widevine download path via audible-rs — matches Audible’s Android L3 stack.
//!
//! Audible Widevine acquire flow:
//! 1. `licenserequest` with `drm_types: ["Widevine","Mpeg"]` (Android account)
//! 2. DASH MPD → one CENC fragmented MP4 (`BaseURL`) + PSSH
//! 3. CDM challenge → `drmlicense` → content key (KID/key)
//! 4. Ranged download of the fMP4, then native DASH/CENC decrypt
//!
//! Codecs: AAC-LC always; optional xHE-AAC. Spatial/Atmos needs L1 and is not
//! requested by acquire on desktop.

use std::path::{Path, PathBuf};

use audible_rs::api::client::Client;
use audible_rs::downloader::{
    self, download_cenc_to_file, request_drmlicense, request_widevine_license, Quality,
    WidevineGrant,
};
use audible_rs::widevine::{mpd, provider, Cdm, Device};
use bookclerk_config::AudioQuality;

use crate::error::{AudibleError, Result};

/// Amazon Android device type id required for Widevine drmlicense grants.
const ANDROID_DEVICE_TYPE: &str = "A10KISP2GWF0E4";

/// Classic Libation AudibleCdm endpoint — provisions a unique L3 `.wvd` per account.
pub const DEFAULT_WIDEVINE_CDM_PROVIDER: &str =
    "https://ollj0gz40d.execute-api.us-west-2.amazonaws.com/default/AudibleCdm";

/// Loaded Widevine CDM client.
pub struct WidevineCdm {
    cdm: Cdm,
    pub security_level: u8,
}

/// Resolve provider URL: `None` config → built-in Bookclerk AudibleCdm; empty/`off` → disabled.
#[must_use]
pub fn effective_cdm_provider(configured: Option<&str>) -> Option<&str> {
    match configured {
        None => Some(DEFAULT_WIDEVINE_CDM_PROVIDER),
        Some(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("off") {
                None
            } else {
                Some(trimmed)
            }
        }
    }
}

/// Resolve and load a `.wvd` device blob (local files only).
///
/// Search order:
/// 1. Explicit `output.widevine_cdm` path (absolute, or relative to `files_dir`)
/// 2. `{files_dir}/widevine.wvd`
/// 3. `{files_dir}/Accounts/{account_stem}.wvd`
pub fn load_widevine_cdm(
    files_dir: &Path,
    configured: Option<&Path>,
    account_stem: Option<&str>,
) -> Result<(WidevineCdm, PathBuf)> {
    let candidates = cdm_candidates(files_dir, configured, account_stem);
    let mut last_err = None;
    for path in candidates {
        if !path.exists() {
            continue;
        }
        match load_cdm_at(&path) {
            Ok(cdm) => return Ok((cdm, path)),
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        AudibleError::Widevine(
            "no Widevine CDM (.wvd) found — set output.widevine_cdm, place \
             widevine.wvd under BOOKCLERK_FILES_DIR, or Accounts/<account>.wvd"
                .into(),
        )
    }))
}

/// Load a local L3 CDM, or auto-provision one via the classic Libation AudibleCdm provider.
///
/// Widevine drmlicense requires an **Android**-registered account
/// (`bookclerk auth login` always registers as Android). Spatial/Atmos (L1) remains unavailable.
pub async fn ensure_widevine_cdm(
    files_dir: &Path,
    configured: Option<&Path>,
    account_stem: Option<&str>,
    auth_file: &Path,
    provider_url: Option<&str>,
) -> Result<(WidevineCdm, PathBuf)> {
    match load_widevine_cdm(files_dir, configured, account_stem) {
        Ok(found) => return Ok(found),
        Err(err) => {
            if effective_cdm_provider(provider_url).is_none() {
                return Err(err);
            }
            tracing::debug!(error = %err, "local Widevine CDM missing; trying provider");
        }
    }

    let Some(endpoint) = effective_cdm_provider(provider_url) else {
        unreachable!("checked above");
    };

    let dest = account_stem
        .map(|stem| crate::paths::widevine_cdm_file_for(files_dir, stem))
        .unwrap_or_else(|| files_dir.join("widevine.wvd"));

    provision_cdm_from_provider(auth_file, endpoint, &dest).await?;
    load_cdm_at(&dest).map(|cdm| (cdm, dest))
}

async fn provision_cdm_from_provider(auth_file: &Path, endpoint: &str, dest: &Path) -> Result<()> {
    let auth = crate::auth::load_authenticator(auth_file, None).await?;
    let device_type = auth.device_type().unwrap_or("unknown");
    if device_type != ANDROID_DEVICE_TYPE {
        return Err(AudibleError::Widevine(format!(
            "Widevine L3 needs an Android-registered account (device_type={device_type:?}). \
             Re-login with: bookclerk auth login --force"
        )));
    }
    let signer = auth.signer().cloned().ok_or_else(|| {
        AudibleError::Widevine(
            "account has no signing material; re-login with bookclerk auth login --force".into(),
        )
    })?;
    let api_url = format!(
        "https://api.audible.{}/1.0/account/information",
        auth.locale().domain
    );
    let signed = tokio::task::spawn_blocking(move || {
        signer.sign_request("GET", "/1.0/account/information", b"")
    })
    .await
    .map_err(|err| AudibleError::Widevine(format!("CDM sign task failed: {err}")))?;

    tracing::info!(endpoint, dest = %dest.display(), "provisioning Widevine L3 CDM");
    let wvd = provider::fetch_wvd(endpoint, &api_url, &signed)
        .await
        .map_err(|err| AudibleError::Widevine(format!("CDM provider failed: {err}")))?;
    Device::from_wvd(&wvd).map_err(|err| {
        AudibleError::Widevine(format!("CDM provider returned invalid .wvd: {err}"))
    })?;

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_private_file(dest, &wvd)?;
    tracing::info!(
        path = %dest.display(),
        bytes = wvd.len(),
        "Widevine L3 CDM provisioned and cached"
    );
    Ok(())
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path).map_err(|err| {
        AudibleError::Widevine(format!("failed to write CDM {}: {err}", path.display()))
    })?;
    file.write_all(bytes).map_err(|err| {
        AudibleError::Widevine(format!("failed to write CDM {}: {err}", path.display()))
    })?;
    Ok(())
}

fn cdm_candidates(
    files_dir: &Path,
    configured: Option<&Path>,
    account_stem: Option<&str>,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(path) = configured {
        if path.is_absolute() {
            out.push(path.to_path_buf());
        } else {
            out.push(files_dir.join(path));
        }
    }
    out.push(files_dir.join("widevine.wvd"));
    if let Some(stem) = account_stem {
        out.push(crate::paths::widevine_cdm_file_for(files_dir, stem));
    }
    out
}

fn load_cdm_at(path: &Path) -> Result<WidevineCdm> {
    let bytes = std::fs::read(path).map_err(|err| {
        AudibleError::Widevine(format!("failed to read CDM {}: {err}", path.display()))
    })?;
    let device = Device::from_wvd(&bytes).map_err(|err| {
        AudibleError::Widevine(format!("failed to parse CDM {}: {err}", path.display()))
    })?;
    let security_level = device.security_level();
    let cdm = Cdm::from_device(&device).map_err(|err| {
        AudibleError::Widevine(format!("failed to init CDM {}: {err}", path.display()))
    })?;
    Ok(WidevineCdm {
        cdm,
        security_level,
    })
}

fn to_quality(q: AudioQuality) -> Quality {
    match q {
        AudioQuality::High => Quality::High,
        AudioQuality::Normal => Quality::Normal,
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Result of a Widevine (or Mpeg-fallback) download into `work_dir`.
#[derive(Debug, Clone)]
pub struct WidevineDownload {
    pub path: PathBuf,
    pub drm_type: String,
    pub content_format: Option<String>,
    pub content_size: Option<u64>,
    /// CENC key id (hex); absent for plain Mpeg.
    pub kid: Option<String>,
    /// CENC content key (hex).
    pub key: Option<String>,
    pub needs_decrypt: bool,
    pub pdf_url: Option<String>,
}

#[derive(Debug, Clone)]
struct CachedKey {
    kid: String,
    key: String,
}

/// Request Widevine license, fetch MPD, obtain content key, download CENC (or plain MP3).
#[allow(clippy::too_many_arguments)]
pub async fn fetch_widevine_download(
    client: &Client,
    marketplace: &str,
    asin: &str,
    quality: AudioQuality,
    xhe: bool,
    spatial: bool,
    cdm: &WidevineCdm,
    work_dir: &Path,
) -> Result<WidevineDownload> {
    let spatial = if spatial && cdm.security_level != 1 {
        tracing::warn!(
            asin,
            level = cdm.security_level,
            "Dolby Atmos needs Widevine L1; downloading stereo instead"
        );
        false
    } else {
        spatial
    };

    let grant =
        request_widevine_license(client, marketplace, asin, to_quality(quality), spatial, xhe)
            .await
            .map_err(AudibleError::from)?;

    match grant {
        WidevineGrant::Mpeg(mpeg) => {
            tracing::info!(asin, "Widevine request fell back to plain Mpeg download");
            let dest = work_dir.join(format!("{asin}.mp3"));
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let (_outcome, path) = download_cenc_to_file(
                &mpeg.offline_url,
                &dest,
                mpeg.content_size,
                true,
                None,
                &["audio/mpeg", "audio/mp3"],
                None,
            )
            .await
            .map_err(|err| AudibleError::Download(err.to_string()))?;
            Ok(WidevineDownload {
                path,
                drm_type: "Mpeg".into(),
                content_format: Some(mpeg.content_format),
                content_size: mpeg.content_size,
                kid: None,
                key: None,
                needs_decrypt: false,
                pdf_url: None,
            })
        }
        WidevineGrant::Widevine(license) => {
            let pdf_url = license.pdf_url.clone();
            let mpd_xml = fetch_text(&license.mpd_url).await?;
            let stream = mpd::parse(&mpd_xml, &license.mpd_url).map_err(|err| {
                AudibleError::Widevine(format!("MPD parse failed for {asin}: {err}"))
            })?;

            let enc_path = work_dir.join(format!(
                "{asin}.{}.cenc",
                stream.content_format.replace('/', "_")
            ));
            if let Some(parent) = enc_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }

            let cached = obtain_content_key(
                client,
                marketplace,
                asin,
                &cdm.cdm,
                &stream.pssh_init_data,
                &enc_path,
            )
            .await?;

            let (_outcome, path) = download_cenc_to_file(
                &stream.content_url,
                &enc_path,
                license.content_size,
                true,
                None,
                &["audio/mp4", "video/mp4"],
                license.version_tag().as_deref(),
            )
            .await
            .map_err(|err| AudibleError::Download(err.to_string()))?;

            Ok(WidevineDownload {
                path,
                drm_type: "Widevine".into(),
                content_format: Some(stream.content_format),
                content_size: license.content_size,
                kid: Some(cached.kid),
                key: Some(cached.key),
                needs_decrypt: true,
                pdf_url,
            })
        }
    }
}

async fn obtain_content_key(
    client: &Client,
    marketplace: &str,
    asin: &str,
    cdm: &Cdm,
    pssh_init_data: &[u8],
    enc_path: &Path,
) -> Result<CachedKey> {
    let wvkey_path = enc_path.with_extension("wvkey");
    if let Some(cached) = read_wvkey(&wvkey_path) {
        return Ok(cached);
    }

    let challenge = cdm
        .challenge(pssh_init_data, true)
        .map_err(|err| AudibleError::Widevine(format!("CDM challenge failed: {err}")))?;
    let license = request_drmlicense(client, marketplace, asin, &challenge.message)
        .await
        .map_err(AudibleError::from)?;
    let key = cdm
        .parse_license(&challenge, &license)
        .map_err(|err| AudibleError::Widevine(format!("CDM license parse failed: {err}")))?
        .into_iter()
        .next()
        .ok_or_else(|| AudibleError::Widevine("license returned no content key".into()))?;
    let cached = CachedKey {
        kid: hex_encode(&key.kid),
        key: hex_encode(&key.key),
    };
    let _ = write_wvkey(&wvkey_path, &cached);
    Ok(cached)
}

fn read_wvkey(path: &Path) -> Option<CachedKey> {
    #[derive(serde::Deserialize)]
    struct KidKey {
        kid: String,
        key: String,
    }
    let parsed: KidKey = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    if hex_decode(&parsed.kid)?.len() != 16 || hex_decode(&parsed.key)?.len() != 16 {
        return None;
    }
    Some(CachedKey {
        kid: parsed.kid,
        key: parsed.key,
    })
}

fn write_wvkey(path: &Path, key: &CachedKey) -> Result<()> {
    let json = serde_json::json!({
        "kid": key.kid,
        "key": key.key,
    })
    .to_string();
    std::fs::write(path, json).map_err(AudibleError::from)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

async fn fetch_text(url: &str) -> Result<String> {
    let text = downloader::plain_http_client()
        .map_err(|err| AudibleError::Download(err.to_string()))?
        .get(url)
        .header(reqwest::header::USER_AGENT, downloader::CENC_USER_AGENT)
        .send()
        .await
        .map_err(|err| AudibleError::Download(err.to_string()))?
        .error_for_status()
        .map_err(|err| AudibleError::Download(err.to_string()))?
        .text()
        .await
        .map_err(|err| AudibleError::Download(err.to_string()))?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdm_candidates_prefer_configured() {
        let files = PathBuf::from("/data");
        let configured = PathBuf::from("custom.wvd");
        let list = cdm_candidates(&files, Some(&configured), Some("alice"));
        assert_eq!(list[0], PathBuf::from("/data/custom.wvd"));
        assert!(list.iter().any(|p| p.ends_with("widevine.wvd")));
        assert!(list.iter().any(|p| p.ends_with("Accounts/alice.wvd")));
        assert!(!list.iter().any(|p| p.ends_with("auth/alice.wvd")));
    }

    #[test]
    fn provider_defaults_and_off() {
        assert_eq!(
            effective_cdm_provider(None),
            Some(DEFAULT_WIDEVINE_CDM_PROVIDER)
        );
        assert_eq!(effective_cdm_provider(Some("off")), None);
        assert_eq!(effective_cdm_provider(Some("")), None);
        assert_eq!(
            effective_cdm_provider(Some("https://example.test/cdm")),
            Some("https://example.test/cdm")
        );
    }

    #[test]
    fn hex_roundtrip() {
        let bytes = [0xabu8, 0xcd, 0x11];
        assert_eq!(hex_decode(&hex_encode(&bytes)).as_deref(), Some(&bytes[..]));
    }
}
