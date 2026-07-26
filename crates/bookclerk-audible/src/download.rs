//! License request + encrypted download via audible-rs (Adrm + Widevine).

use std::path::{Path, PathBuf};

use audible_rs::api::client::Client;
use audible_rs::downloader::{self, Quality};
use audible_rs::models::content::DownloadLicense;
use bookclerk_config::AudioQuality;
use serde::{Deserialize, Serialize};

use crate::accounts::resolve_auth_file_async;
use crate::auth::load_authenticator;
use crate::error::{AudibleError, Result};
use crate::options::DownloadOptions;
use crate::widevine::{ensure_widevine_cdm, fetch_widevine_download};

/// Authenticated Audible client bound to one account.
pub struct AccountClient {
    pub client: Client,
    pub account_id: String,
    pub marketplace: String,
    pub auth_file: PathBuf,
}

/// DRM / container kind produced by download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrmKind {
    /// Adrm aaxc with voucher key/iv.
    Adrm,
    /// Widevine CENC with kid/key.
    Widevine,
    /// Plain Mpeg (mp3) — no decrypt.
    Mpeg,
}

/// Public summary of a license grant (no secrets).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LicenseSummary {
    pub asin: String,
    pub status_code: String,
    pub drm_type: Option<String>,
    pub content_format: Option<String>,
    pub content_size: Option<u64>,
    pub granted: bool,
    pub denial_message: Option<String>,
    pub has_voucher: bool,
    pub offline_url_present: bool,
}

/// Encrypted (or plain Mpeg) audio downloaded to cache, plus decrypt material.
#[derive(Debug, Clone)]
pub struct EncryptedDownload {
    pub path: PathBuf,
    pub drm_type: Option<String>,
    pub content_format: Option<String>,
    pub content_size: Option<u64>,
    pub drm_kind: DrmKind,
    /// Adrm aaxc content key (hex).
    pub key: Option<String>,
    /// Adrm aaxc IV (hex).
    pub iv: Option<String>,
    /// Widevine CENC key id (hex).
    pub kid: Option<String>,
    /// Widevine CENC content key (hex).
    pub cenc_key: Option<String>,
    /// True when the file still needs decrypt (native Adrm or CENC).
    pub needs_decrypt: bool,
    pub pdf_url: Option<String>,
}

/// Open an audible-rs [`Client`] for `account` (auth file stem, label, or customer id).
pub async fn open_account_client(files_dir: &Path, account: &str) -> Result<AccountClient> {
    let auth_file = resolve_auth_file_async(files_dir, account).await?;
    let auth = load_authenticator(&auth_file, None).await?;
    let marketplace = auth.locale().country_code.to_string();
    let account_id = auth.customer_id().map(str::to_string).unwrap_or_else(|| {
        crate::paths::auth_stem_from_path(&auth_file).unwrap_or_else(|| account.to_string())
    });
    let client = Client::new(auth).map_err(AudibleError::from)?;
    Ok(AccountClient {
        client,
        account_id,
        marketplace,
        auth_file,
    })
}

/// Request an Adrm/Mpeg download license for `asin`.
pub async fn request_content_license(
    client: &Client,
    marketplace: &str,
    asin: &str,
    quality: AudioQuality,
) -> Result<DownloadLicense> {
    let q = match quality {
        AudioQuality::High => Quality::High,
        AudioQuality::Normal => Quality::Normal,
    };
    downloader::request_license(client, marketplace, asin, q)
        .await
        .map_err(AudibleError::from)
}

/// Full license API JSON (classic `get-license` without summary mode).
#[must_use]
pub fn license_full_json(license: &DownloadLicense) -> String {
    serde_json::to_string_pretty(&license.raw).unwrap_or_else(|_| "{}".into())
}

/// Summarize a license without exposing voucher / URL query secrets.
#[must_use]
pub fn summarize_license(license: &DownloadLicense) -> LicenseSummary {
    LicenseSummary {
        asin: license.asin.clone(),
        status_code: license.status_code.clone(),
        drm_type: license.drm_type.clone(),
        content_format: license.content_format.clone(),
        content_size: license.content_size,
        granted: license.is_granted(),
        denial_message: license.denial_message.clone(),
        has_voucher: license.has_voucher,
        offline_url_present: license.offline_url.is_some(),
    }
}

/// Download the licensed Adrm/Mpeg audio file to `dest` (parent dirs created).
pub async fn download_licensed_audio(
    client: &Client,
    license: &DownloadLicense,
    dest: &Path,
    speed_limit_kbps: u32,
) -> Result<EncryptedDownload> {
    if !license.is_granted() {
        return Err(AudibleError::License(format!(
            "license denied for {}: {}",
            license.asin,
            license
                .denial_message
                .as_deref()
                .unwrap_or("no reason given")
        )));
    }

    let url = license
        .offline_url
        .as_deref()
        .ok_or_else(|| AudibleError::License(format!("{}: missing offline_url", license.asin)))?;

    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let content_types = [
        "audio/aax",
        "audio/vnd.audible.aax",
        "audio/mpeg",
        "audio/mp3",
        "audio/mp4",
        "audio/x-m4a",
        "audio/audible",
    ];
    let ext_overrides = [
        ("audio/mpeg", "mp3"),
        ("audio/mp3", "mp3"),
        ("audio/mp4", "m4a"),
        ("audio/x-m4a", "m4a"),
    ];
    let (_outcome, path) = if speed_limit_kbps > 0 {
        crate::throttle::download_to_file_limited(
            client,
            url,
            dest,
            license.content_size,
            false,
            speed_limit_kbps,
            &content_types,
            &ext_overrides,
            license.version_tag().as_deref(),
        )
        .await?
    } else {
        downloader::download_to_file(
            client,
            url,
            dest,
            license.content_size,
            false,
            None,
            &content_types,
            &ext_overrides,
            license.version_tag().as_deref(),
        )
        .await
        .map_err(|err| AudibleError::Download(err.to_string()))?
    };

    let drm = license.drm_type.as_deref().unwrap_or("Adrm");
    let is_mpeg = drm.eq_ignore_ascii_case("Mpeg");
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let plain_media = matches!(ext.as_str(), "mp3" | "m4a");

    let (key, iv, needs_decrypt, drm_kind) = if is_mpeg || plain_media {
        (None, None, false, DrmKind::Mpeg)
    } else if license.has_voucher {
        let (dt, ds, cid) = (
            client.device_type(),
            client.device_serial(),
            client.customer_id(),
        );
        let (Some(dt), Some(ds), Some(cid)) = (dt, ds, cid) else {
            return Err(AudibleError::License(
                "auth file lacks device/customer data needed to decrypt the aaxc voucher".into(),
            ));
        };
        let voucher = license
            .decrypt_voucher(dt, ds, cid)
            .map_err(|err| AudibleError::License(format!("voucher decrypt failed: {err}")))?;
        (Some(voucher.key), Some(voucher.iv), true, DrmKind::Adrm)
    } else {
        return Err(AudibleError::License(format!(
            "{}: Adrm grant has no voucher (cannot decrypt)",
            license.asin
        )));
    };

    Ok(EncryptedDownload {
        path,
        drm_type: license.drm_type.clone(),
        content_format: license.content_format.clone(),
        content_size: license.content_size,
        drm_kind,
        key,
        iv,
        kid: None,
        cenc_key: None,
        needs_decrypt,
        pdf_url: license.pdf_url.clone(),
    })
}

/// Parse a license JSON file (API response or classic get-license output).
pub fn parse_license_json(text: &str) -> Result<DownloadLicense> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|err| AudibleError::License(format!("invalid license JSON: {err}")))?;

    if let Some(license) = DownloadLicense::from_response(value.clone()) {
        return Ok(license);
    }

    // Classic get-license / LicenseInfo wrapper.
    if let Some(inner) = value.get("content_license") {
        if let Some(license) = DownloadLicense::from_response(serde_json::json!({
            "content_license": inner
        })) {
            return Ok(license);
        }
    }
    if let Some(inner) = value
        .get("ContentMetadata")
        .and_then(|m| m.get("content_license"))
    {
        if let Some(license) = DownloadLicense::from_response(serde_json::json!({
            "content_license": inner
        })) {
            return Ok(license);
        }
    }

    Err(AudibleError::License(
        "could not parse license JSON (expected content_license)".into(),
    ))
}

/// Fetch license + download audio into `cache_dir` for acquire.
///
/// Strategy:
/// - If `options.widevine`: Widevine first (CDM required unless Mpeg fallback).
/// - Else try Adrm; on 000307 automatically fall back to Widevine when a CDM is available.
pub async fn fetch_and_download(
    files_dir: &Path,
    account: &str,
    asin: &str,
    quality: AudioQuality,
    cache_dir: &Path,
) -> Result<(AccountClient, EncryptedDownload, LicenseSummary)> {
    let options = DownloadOptions {
        quality,
        ..DownloadOptions::default()
    };
    fetch_and_download_with_options(files_dir, account, asin, &options, cache_dir).await
}

/// Like [`fetch_and_download`] but honors Widevine / xHE-AAC options.
pub async fn fetch_and_download_with_options(
    files_dir: &Path,
    account: &str,
    asin: &str,
    options: &DownloadOptions,
    cache_dir: &Path,
) -> Result<(AccountClient, EncryptedDownload, LicenseSummary)> {
    let account_client = open_account_client(files_dir, account).await?;
    let auth_stem = crate::paths::auth_stem_from_path(&account_client.auth_file);

    if options.widevine {
        return fetch_via_widevine(
            account_client,
            asin,
            options,
            cache_dir,
            files_dir,
            auth_stem.as_deref(),
            true,
        )
        .await;
    }

    match request_content_license(
        &account_client.client,
        &account_client.marketplace,
        asin,
        options.quality,
    )
    .await
    {
        Ok(license) => {
            let summary = summarize_license(&license);
            if !summary.granted {
                return Err(AudibleError::License(format!(
                    "license denied for {asin}: {}",
                    summary
                        .denial_message
                        .as_deref()
                        .unwrap_or("no reason given")
                )));
            }
            let format = license
                .content_format
                .as_deref()
                .filter(|f| !f.is_empty())
                .unwrap_or("audio");
            let dest = cache_dir.join(asin).join(format!("{asin}.{format}.aaxc"));
            let download = download_licensed_audio(
                &account_client.client,
                &license,
                &dest,
                options.download_speed_limit_kbps,
            )
            .await?;
            Ok((account_client, download, summary))
        }
        Err(err) if err.is_no_aaxc_asset() => {
            tracing::info!(
                asin,
                "Adrm unavailable (000307); falling back to Widevine/CENC"
            );
            fetch_via_widevine(
                account_client,
                asin,
                options,
                cache_dir,
                files_dir,
                auth_stem.as_deref(),
                false,
            )
            .await
        }
        Err(err) => Err(err),
    }
}

async fn fetch_via_widevine(
    account_client: AccountClient,
    asin: &str,
    options: &DownloadOptions,
    cache_dir: &Path,
    files_dir: &Path,
    auth_stem: Option<&str>,
    forced: bool,
) -> Result<(AccountClient, EncryptedDownload, LicenseSummary)> {
    let (cdm, cdm_path) = ensure_widevine_cdm(
        files_dir,
        options.widevine_cdm.as_deref(),
        auth_stem,
        &account_client.auth_file,
        options.widevine_cdm_provider.as_deref(),
    )
    .await?;
    tracing::info!(
        asin,
        cdm = %cdm_path.display(),
        forced,
        xhe = options.xhe_aac,
        "starting Widevine acquire path"
    );

    let work_dir = cache_dir.join(asin);
    let wv = fetch_widevine_download(
        &account_client.client,
        &account_client.marketplace,
        asin,
        options.quality,
        options.xhe_aac,
        false,
        &cdm,
        &work_dir,
    )
    .await?;

    let summary = LicenseSummary {
        asin: asin.to_string(),
        status_code: "Granted".into(),
        drm_type: Some(wv.drm_type.clone()),
        content_format: wv.content_format.clone(),
        content_size: wv.content_size,
        granted: true,
        denial_message: None,
        has_voucher: wv.kid.is_some(),
        offline_url_present: true,
    };

    let drm_kind = if wv.drm_type.eq_ignore_ascii_case("Mpeg") {
        DrmKind::Mpeg
    } else {
        DrmKind::Widevine
    };

    let download = EncryptedDownload {
        path: wv.path,
        drm_type: Some(wv.drm_type),
        content_format: wv.content_format,
        content_size: wv.content_size,
        drm_kind,
        key: None,
        iv: None,
        kid: wv.kid,
        cenc_key: wv.key,
        needs_decrypt: wv.needs_decrypt,
        pdf_url: wv.pdf_url,
    };

    Ok((account_client, download, summary))
}

#[cfg(test)]
mod tests {
    use super::*;
    use audible_rs::models::content::DownloadLicense;

    #[test]
    fn summarize_granted_license() {
        let license = DownloadLicense::from_response(serde_json::json!({
            "content_license": {
                "status_code": "Granted",
                "asin": "B00TEST",
                "drm_type": "Adrm",
                "license_response": "VOUCHER",
                "content_metadata": {
                    "content_url": {"offline_url": "https://cds.example/x.aaxc?Policy=abc"},
                    "content_reference": {
                        "content_format": "AAX_44_64",
                        "content_size_in_bytes": 100u64
                    }
                }
            }
        }))
        .unwrap();
        let summary = summarize_license(&license);
        assert!(summary.granted);
        assert!(summary.has_voucher);
        assert!(summary.offline_url_present);
        assert_eq!(summary.drm_type.as_deref(), Some("Adrm"));
        assert_eq!(summary.content_format.as_deref(), Some("AAX_44_64"));
    }
}
