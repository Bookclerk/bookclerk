//! License request + encrypted download via audible-rs (Adrm + Widevine).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use audible_rs::api::client::Client;
use audible_rs::downloader::{self, Quality};
use audible_rs::models::content::DownloadLicense;
use bookclerk_config::AudioQuality;
use bookclerk_library::SourceScope;
use serde::{Deserialize, Serialize};

use crate::error::{AudibleError, Result};
use crate::options::DownloadOptions;
use crate::widevine::{ensure_widevine_cdm, fetch_widevine_download};

/// Authenticated Audible client bound to one account.
///
/// Cloneable (cheap — shares the inner `Arc<Client>`). Any token refresh
/// propagates to all clones because they share the same `Arc<Mutex<Authenticator>>`.
#[derive(Clone)]
pub struct AccountClient {
    /// HTTP or RPC client used for outbound requests.
    pub client: Arc<Client>,
    /// Account Identifier.
    pub account_id: String,
    /// Marketplace.
    pub marketplace: String,
}

impl std::fmt::Debug for AccountClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountClient")
            .field("account_id", &self.account_id)
            .field("marketplace", &self.marketplace)
            .finish_non_exhaustive()
    }
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
    /// Amazon ASIN identifier.
    pub asin: String,
    /// Status code.
    pub status_code: String,
    /// DRM type.
    pub drm_type: Option<String>,
    /// Content format.
    pub content_format: Option<String>,
    /// Content size.
    pub content_size: Option<u64>,
    /// Granted.
    pub granted: bool,
    /// Denial message.
    pub denial_message: Option<String>,
    /// Has voucher.
    pub has_voucher: bool,
    /// Offline URL present.
    pub offline_url_present: bool,
}

/// Encrypted (or plain Mpeg) audio downloaded to cache, plus decrypt material.
#[derive(Debug, Clone)]
pub struct EncryptedDownload {
    /// Filesystem path for this value.
    pub path: PathBuf,
    /// DRM type.
    pub drm_type: Option<String>,
    /// Content format.
    pub content_format: Option<String>,
    /// Content size.
    pub content_size: Option<u64>,
    /// DRM kind.
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
    /// Pdf URL.
    pub pdf_url: Option<String>,
}

/// Open an audible-rs [`Client`] for `account` (customer id, or display label).
///
/// Credentials are keyed by Audible customer id. A display label is resolved
/// via the scoped accounts table. Clients are cached process-wide (same idea
/// as the library unseal cache) so batch acquire does not re-open auth for
/// every title — no special-casing in the acquire job.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn open_account_client(scope: &SourceScope, account: &str) -> Result<AccountClient> {
    if let Some(cached) = client_cache_get(account) {
        return Ok(cached);
    }
    let auth = load_auth_resolving_label(scope, account)
        .await?
        .ok_or_else(|| {
            AudibleError::Auth(format!(
                "no Audible credentials in encrypted_secrets for `{account}`"
            ))
        })?;
    let marketplace = auth.locale().country_code.to_string();
    let account_id = auth
        .customer_id()
        .map(str::to_string)
        .unwrap_or_else(|| account.to_string());
    let client = Client::new(auth).map_err(AudibleError::from)?;
    let opened = AccountClient {
        client: Arc::new(client),
        account_id: account_id.clone(),
        marketplace,
    };
    client_cache_put(account, &opened);
    if account != account_id {
        client_cache_put(&account_id, &opened);
    }
    Ok(opened)
}

/// Drop cached clients for `account` (after login / revoke / credential rewrite).
pub fn invalidate_account_client_cache(account: &str) {
    if let Ok(mut guard) = account_client_cache().lock() {
        guard.remove(account);
    }
}

/// Load auth by customer id, or by display label via the accounts table.
async fn load_auth_resolving_label(
    scope: &SourceScope,
    account: &str,
) -> Result<Option<audible_rs::auth::Authenticator>> {
    if let Some(auth) = crate::db::load_authenticator_from_db(scope, account).await? {
        return Ok(Some(auth));
    }
    let accounts = scope.list_accounts().await?;
    for acct in accounts {
        let label_match = acct
            .label
            .as_deref()
            .is_some_and(|l| l.eq_ignore_ascii_case(account));
        if label_match || acct.account_id.eq_ignore_ascii_case(account) {
            if let Some(auth) =
                crate::db::load_authenticator_from_db(scope, &acct.account_id).await?
            {
                return Ok(Some(auth));
            }
        }
    }
    Ok(None)
}

/// Process-wide cache of Audible HTTP clients keyed by account id.
fn account_client_cache() -> &'static Mutex<HashMap<String, AccountClient>> {
    static CACHE: OnceLock<Mutex<HashMap<String, AccountClient>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Returns a cloned cached client for `account`, or `None` if the lock or entry is missing.
fn client_cache_get(account: &str) -> Option<AccountClient> {
    account_client_cache().lock().ok()?.get(account).cloned()
}

/// Stores a client for `account`; silently skips if the cache lock is poisoned.
fn client_cache_put(account: &str, client: &AccountClient) {
    if let Ok(mut guard) = account_client_cache().lock() {
        guard.insert(account.to_string(), client.clone());
    }
}

/// Request an Adrm/Mpeg download license for `asin`.
///
/// # Errors
///
/// Returns an error when the operation fails.
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
///
/// # Errors
///
/// Returns an error when the operation fails.
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
///
/// # Errors
///
/// Returns an error when the operation fails.
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
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn fetch_and_download(
    scope: &SourceScope,
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
    fetch_and_download_with_options(scope, files_dir, account, asin, &options, cache_dir).await
}

/// Like [`fetch_and_download_with_options`] but uses a pre-opened `AccountClient`,
/// avoiding a repeated DB lookup + decryption when the caller maintains a per-job cache.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn fetch_and_download_with_client(
    account_client: AccountClient,
    files_dir: &Path,
    asin: &str,
    options: &DownloadOptions,
    cache_dir: &Path,
    scope: Option<&SourceScope>,
) -> Result<(AccountClient, EncryptedDownload, LicenseSummary)> {
    let auth_stem = account_client.account_id.clone();

    if options.widevine {
        return fetch_via_widevine(
            account_client,
            asin,
            options,
            cache_dir,
            files_dir,
            Some(auth_stem.as_str()),
            true,
            scope,
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
                Some(auth_stem.as_str()),
                false,
                scope,
            )
            .await
        }
        Err(err) => Err(err),
    }
}

/// Like [`fetch_and_download`] but honors Widevine / xHE-AAC options.
///
/// Auth and Widevine CDM bytes come from `encrypted_secrets`. `files_dir` is
/// only used for temporary cache / optional BYO CDM path resolution.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn fetch_and_download_with_options(
    scope: &SourceScope,
    files_dir: &Path,
    account: &str,
    asin: &str,
    options: &DownloadOptions,
    cache_dir: &Path,
) -> Result<(AccountClient, EncryptedDownload, LicenseSummary)> {
    let account_client = open_account_client(scope, account).await?;
    let auth_stem = account_client.account_id.clone();

    if options.widevine {
        return fetch_via_widevine(
            account_client,
            asin,
            options,
            cache_dir,
            files_dir,
            Some(auth_stem.as_str()),
            true,
            Some(scope),
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
                Some(auth_stem.as_str()),
                false,
                Some(scope),
            )
            .await
        }
        Err(err) => Err(err),
    }
}

#[allow(clippy::too_many_arguments)]
/// Downloads via Widevine L3 after ensuring a CDM, returning the encrypted payload and license summary.
async fn fetch_via_widevine(
    account_client: AccountClient,
    asin: &str,
    options: &DownloadOptions,
    cache_dir: &Path,
    files_dir: &Path,
    auth_stem: Option<&str>,
    forced: bool,
    scope: Option<&SourceScope>,
) -> Result<(AccountClient, EncryptedDownload, LicenseSummary)> {
    let (cdm, cdm_path) = ensure_widevine_cdm(
        files_dir,
        options.widevine_cdm.as_deref(),
        auth_stem,
        options.widevine_cdm_provider.as_deref(),
        scope,
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
