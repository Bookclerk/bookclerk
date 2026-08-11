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
use audible_rs::auth::Authenticator;
use audible_rs::downloader::{
    self, download_cenc_to_file, request_drmlicense, request_widevine_license, Quality,
    WidevineGrant,
};
use audible_rs::widevine::{mpd, provider, Cdm, Device};
use bookclerk_config::AudioQuality;
use bookclerk_library::SourceScope;

use crate::db::{load_widevine_cdm_from_db, save_widevine_cdm_to_db};
use crate::error::{AudibleError, Result};

/// Amazon Android device type id required for Widevine drmlicense grants.
const ANDROID_DEVICE_TYPE: &str = "A10KISP2GWF0E4";

/// Classic Libation AudibleCdm endpoint — provisions a unique L3 `.wvd` per account.
pub const DEFAULT_WIDEVINE_CDM_PROVIDER: &str =
    "https://ollj0gz40d.execute-api.us-west-2.amazonaws.com/default/AudibleCdm";

/// Loaded Widevine CDM client.
pub struct WidevineCdm {
    cdm: Cdm,
    /// Security level.
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

/// Resolve and load a bring-your-own `.wvd` device blob from local files.
///
/// This is a one-shot import path only (no `Accounts/`). Search order:
/// 1. Explicit `output.widevine_cdm` path (absolute, or relative to `files_dir`)
/// 2. `{files_dir}/widevine.wvd`
pub fn load_widevine_cdm(
    files_dir: &Path,
    configured: Option<&Path>,
) -> Result<(WidevineCdm, PathBuf)> {
    let candidates = cdm_candidates(files_dir, configured);
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
            "no Widevine CDM (.wvd) found — set output.widevine_cdm or place \
             widevine.wvd under BOOKCLERK_FILES_DIR"
                .into(),
        )
    }))
}

/// Load an account's Widevine CDM, or auto-provision one via the classic
/// Libation AudibleCdm provider. CDMs are stored in the `encrypted_secrets` DB
/// table only — nothing is written under `Accounts/`.
///
/// Resolution order:
/// 1. `encrypted_secrets` (DB) for `account_stem`
/// 2. BYO `.wvd` file (`output.widevine_cdm` / `{files_dir}/widevine.wvd`),
///    imported into the DB when possible
/// 3. Provision a fresh L3 CDM from the provider (auth material from the DB) and
///    persist it to the DB
///
/// Widevine drmlicense requires an **Android**-registered account
/// (`bookclerk auth login` always registers as Android). Spatial/Atmos (L1) remains unavailable.
pub async fn ensure_widevine_cdm(
    files_dir: &Path,
    configured: Option<&Path>,
    account_stem: Option<&str>,
    provider_url: Option<&str>,
    scope: Option<&SourceScope>,
) -> Result<(WidevineCdm, PathBuf)> {
    // 1. Try DB when available.
    if let (Some(lib), Some(stem)) = (scope, account_stem) {
        match load_widevine_cdm_from_db(lib, stem).await {
            Ok(Some(bytes)) => {
                tracing::debug!(account = %stem, "loaded Widevine CDM from encrypted_secrets");
                return load_cdm_from_bytes(&bytes, stem);
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(error = %err, "DB CDM lookup failed; trying BYO file");
            }
        }
    }

    // 2. Try a bring-your-own local file; import it into the DB for next time.
    match load_widevine_cdm(files_dir, configured) {
        Ok((cdm, path)) => {
            if let (Some(lib), Some(stem)) = (scope, account_stem) {
                if let Ok(bytes) = std::fs::read(&path) {
                    if let Err(e) = save_widevine_cdm_to_db(lib, stem, &bytes).await {
                        tracing::warn!(error = %e, "failed to import BYO Widevine CDM into DB");
                    }
                }
            }
            return Ok((cdm, path));
        }
        Err(err) => {
            if effective_cdm_provider(provider_url).is_none() {
                return Err(err);
            }
            tracing::debug!(error = %err, "no BYO Widevine CDM; provisioning via provider");
        }
    }

    let Some(endpoint) = effective_cdm_provider(provider_url) else {
        unreachable!("checked above");
    };

    // 3. Provision — requires a DB-backed account (auth material + CDM storage).
    let (Some(lib), Some(stem)) = (scope, account_stem) else {
        return Err(AudibleError::Widevine(
            "Widevine CDM provisioning requires a DB-backed Audible account".into(),
        ));
    };
    let auth = crate::db::load_authenticator_from_db(lib, stem)
        .await?
        .ok_or_else(|| {
            AudibleError::Widevine(format!(
                "no Audible credentials in encrypted_secrets for `{stem}` \
                 (needed to provision a Widevine CDM)"
            ))
        })?;

    let wvd = provision_cdm_bytes_from_provider(&auth, endpoint).await?;

    if let Err(e) = save_widevine_cdm_to_db(lib, stem, &wvd).await {
        tracing::warn!(error = %e, "failed to persist provisioned Widevine CDM to DB");
    }
    tracing::info!(
        account = %stem,
        bytes = wvd.len(),
        "Widevine L3 CDM provisioned and stored in encrypted_secrets"
    );
    load_cdm_from_bytes(&wvd, stem)
}

/// Provision a CDM from the remote AudibleCdm provider and return the raw `.wvd` bytes.
async fn provision_cdm_bytes_from_provider(
    auth: &Authenticator,
    endpoint: &str,
) -> Result<Vec<u8>> {
    let device_type = auth.device_type().unwrap_or("unknown");
    if device_type != ANDROID_DEVICE_TYPE {
        return Err(AudibleError::Widevine(format!(
            "Widevine L3 needs an Android-registered account (device_type={device_type:?}). \
             Re-connect Audible from the Bookclerk Accounts UI"
        )));
    }
    let signer = auth.signer().cloned().ok_or_else(|| {
        AudibleError::Widevine(
            "account has no signing material; re-connect Audible from the Bookclerk Accounts UI"
                .into(),
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

    tracing::info!(endpoint, "provisioning Widevine L3 CDM from provider");
    let wvd = provider::fetch_wvd(endpoint, &api_url, &signed)
        .await
        .map_err(|err| AudibleError::Widevine(format!("CDM provider failed: {err}")))?;
    Device::from_wvd(&wvd).map_err(|err| {
        AudibleError::Widevine(format!("CDM provider returned invalid .wvd: {err}"))
    })?;
    Ok(wvd)
}

/// Decode a `WidevineCdm` from raw `.wvd` bytes (no path; suitable for DB-loaded blobs).
fn load_cdm_from_bytes(bytes: &[u8], label: &str) -> Result<(WidevineCdm, PathBuf)> {
    let device = Device::from_wvd(bytes)
        .map_err(|err| AudibleError::Widevine(format!("failed to parse CDM for {label}: {err}")))?;
    let security_level = device.security_level();
    let cdm = Cdm::from_device(&device)
        .map_err(|err| AudibleError::Widevine(format!("failed to init CDM for {label}: {err}")))?;
    Ok((
        WidevineCdm {
            cdm,
            security_level,
        },
        PathBuf::from(format!("<db:{label}>")),
    ))
}

fn cdm_candidates(files_dir: &Path, configured: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(path) = configured {
        if path.is_absolute() {
            out.push(path.to_path_buf());
        } else {
            out.push(files_dir.join(path));
        }
    }
    out.push(files_dir.join("widevine.wvd"));
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
    /// Filesystem path for this value.
    pub path: PathBuf,
    /// DRM type.
    pub drm_type: String,
    /// Content format.
    pub content_format: Option<String>,
    /// Content size.
    pub content_size: Option<u64>,
    /// CENC key id (hex); absent for plain Mpeg.
    pub kid: Option<String>,
    /// CENC content key (hex).
    pub key: Option<String>,
    /// Needs decrypt.
    pub needs_decrypt: bool,
    /// Pdf URL.
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
        let list = cdm_candidates(&files, Some(&configured));
        assert_eq!(list[0], PathBuf::from("/data/custom.wvd"));
        assert!(list.iter().any(|p| p.ends_with("widevine.wvd")));
        assert!(!list.iter().any(|p| p.ends_with("Accounts/alice.wvd")));
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
