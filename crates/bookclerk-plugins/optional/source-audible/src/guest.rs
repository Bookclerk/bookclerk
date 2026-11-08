//! Guest-process helpers for the external Audible source plugin.
//!
//! Credentials are opaque JSON (`authfile_b64`, optional `widevine_b64`). The
//! host seals them; this module never opens the library DB.
//!
//! Note: native HTTP uses `bookclerk_plugin_sdk::http` (CONNECT through the
//! workerd socket proxy). Nested jail is `NetPolicy::Deny`. OAuth listen is
//! host-owned (`callback_ipc`); the guest does not bind TCP.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::drm::{
    decrypt_adrm, decrypt_cenc, join_cache_component, validated_fs_path, CencDecryptRequest,
    DecryptRequest,
};
use audible_rs::api::client::Client;
use audible_rs::auth::login::{self as login_flow, LoginServer};
use audible_rs::auth::Authenticator;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use bookclerk_mp4::{
    brand_durations_from_chapter_info, brand_trim_range, parse_mp4,
    runtime_length_ms_from_chapter_info, track_duration_ms, TrimRange,
};
use bookclerk_plugin_sdk::{
    FetchOptions, LoginParams, LoginResult, PlainFetch, PlainPart, ScanSummary, SourceAccount,
};
use bookclerk_storefront::abi::{credentials_to_bytes, scan_summary};
use serde_json::{json, Value};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::artifacts::{download_cover_jpeg, fetch_chapter_info};
use crate::auth::{
    export_authfile_plain_bytes, login_defaults, login_server_url, session_from_authenticator,
};
use crate::download::{fetch_and_download_with_client, AccountClient, DrmKind};
use crate::error::{AudibleError, Result};
use crate::options::DownloadOptions;
use crate::sync::collect_account_books;
use crate::ID;

/// In-flight LoginServer OAuth waiting for [`guest_login_complete`].
struct PendingGuestLogin {
    /// Operator-facing account label to persist after OAuth completes.
    label: Option<String>,
    /// Background task that finishes LoginServer and registers the device.
    handle: JoinHandle<Result<Authenticator>>,
}

/// Process-wide map of `session_id` → in-flight guest OAuth (host never sees the authenticator).
fn pending_logins() -> &'static Mutex<HashMap<String, PendingGuestLogin>> {
    static PENDING: OnceLock<Mutex<HashMap<String, PendingGuestLogin>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Start LoginServer OAuth; return `(session_id, browser_url)`.
///
/// Completes later via [`guest_login_complete`].
///
/// When the host sets `callback_ipc` + `callback_public_base`, the guest does
/// **not** bind TCP — it connects to the host IPC tunnel and serves LoginServer
/// on forwarded streams (required under Windows AppContainer).
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn guest_login_start(params: &LoginParams) -> Result<(String, String)> {
    let marketplace = if params.marketplace.trim().is_empty() {
        String::from("us")
    } else {
        params.marketplace.trim().to_ascii_lowercase()
    };
    let callback_bind = parse_callback_bind(params.callback_bind.as_deref())?;
    let defaults = login_defaults(&marketplace);
    let timeout = Duration::from_secs(params.timeout_secs.unwrap_or(300));
    let label = params.label.clone();

    let (url, handle) = if let (Some(ipc), Some(public_base)) = (
        params.callback_ipc.as_deref(),
        params.callback_public_base.as_deref(),
    ) {
        let server = LoginServer::prepare(defaults);
        let url = format!(
            "{}{}",
            public_base.trim_end_matches('/'),
            server.landing_path()
        );
        let ipc = ipc.to_string();
        let handle = tokio::spawn(async move {
            let stream = connect_callback_ipc(&ipc).await?;
            let (reader, writer) = tokio::io::split(stream);
            let tunnel = std::sync::Arc::new(tokio::sync::Mutex::new(
                bookclerk_plugin_sdk::TunnelGuest::new(reader, writer),
            ));
            let login = server
                .run_with_accept(timeout, move || {
                    let tunnel = std::sync::Arc::clone(&tunnel);
                    async move {
                        tunnel
                            .lock()
                            .await
                            .accept()
                            .await
                            .map_err(|err| std::io::Error::other(err.to_string()))
                    }
                })
                .await?;
            register_after_login(login).await
        });
        (url, handle)
    } else {
        let server = LoginServer::bind(callback_bind, defaults).await?;
        let (url, _addr) = login_server_url(&server, callback_bind);
        let handle = tokio::spawn(async move {
            let login = server.run(timeout).await?;
            register_after_login(login).await
        });
        (url, handle)
    };

    let session_id = Uuid::new_v4().to_string();
    pending_logins()
        .lock()
        .map_err(|_| AudibleError::Auth("guest login session lock poisoned".into()))?
        .insert(session_id.clone(), PendingGuestLogin { label, handle });

    Ok((session_id, url))
}

/// Exchanges the LoginServer authorization code for an audible-rs [`Authenticator`].
async fn register_after_login(
    login: audible_rs::auth::login::ServerLogin,
) -> Result<Authenticator> {
    let http = audible_rs::HttpClient::builder()
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(|err| AudibleError::Auth(err.to_string()))?;
    let auth = login_flow::register(
        &http,
        &login.locale,
        &login.device,
        &login.pkce,
        &login.code,
        login.with_username,
    )
    .await?;
    Ok(auth)
}

#[cfg(unix)]
/// Connects to the host callback tunnel (Unix socket) so LoginServer can run without binding TCP.
async fn connect_callback_ipc(endpoint: &str) -> Result<tokio::net::UnixStream> {
    tokio::net::UnixStream::connect(endpoint)
        .await
        .map_err(|err| AudibleError::Auth(format!("callback IPC connect {endpoint}: {err}")))
}

#[cfg(windows)]
async fn connect_callback_ipc(
    endpoint: &str,
) -> Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    use tokio::net::windows::named_pipe::ClientOptions;
    ClientOptions::new()
        .open(endpoint)
        .map_err(|err| AudibleError::Auth(format!("callback IPC open {endpoint}: {err}")))
}

/// Await a pending OAuth session and return account + opaque credential bytes.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn guest_login_complete(session_id: &str) -> Result<LoginResult> {
    let pending = pending_logins()
        .lock()
        .map_err(|_| AudibleError::Auth("guest login session lock poisoned".into()))?
        .remove(session_id)
        .ok_or_else(|| {
            AudibleError::Auth(format!(
                "unknown or expired guest login session `{session_id}`"
            ))
        })?;

    let auth = pending
        .handle
        .await
        .map_err(|e| AudibleError::Auth(format!("guest login task failed: {e}")))??;

    let session = session_from_authenticator(&auth, pending.label);
    let credentials = credentials_json_from_auth(&auth, None)?;
    let credentials =
        credentials_to_bytes(&credentials).map_err(|e| AudibleError::Auth(e.to_string()))?;

    Ok(LoginResult {
        account: SourceAccount {
            account_id: session.account_id,
            source: ID.into(),
            marketplace: session.marketplace,
            label: session.label,
            scan_enabled: true,
        },
        credentials: Some(credentials),
    })
}

/// Scan libraries for host-injected credential blobs.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn guest_scan(
    credentials: &BTreeMap<String, Value>,
    account_filter: &[String],
    page_size: u32,
    import_episodes: bool,
    import_plus_titles: bool,
) -> Result<ScanSummary> {
    if credentials.is_empty() {
        return Err(AudibleError::NoAccounts(
            "no Audible credentials from host — run login first".into(),
        ));
    }
    let explicit = !account_filter.is_empty();
    let mut books = Vec::new();
    let mut accounts = 0usize;
    let mut pages = 0u32;

    for (account_id, creds) in credentials {
        if explicit
            && !account_filter
                .iter()
                .any(|n| n.eq_ignore_ascii_case(account_id))
        {
            continue;
        }
        let auth = authenticator_from_credentials(creds)?;
        let marketplace = auth.locale().country_code.to_string();
        let resolved_id = auth
            .customer_id()
            .map(str::to_string)
            .unwrap_or_else(|| account_id.clone());
        let client = Client::new(auth).map_err(AudibleError::from)?;
        let (batch, p) = collect_account_books(
            &client,
            &resolved_id,
            &marketplace,
            page_size,
            import_episodes,
            import_plus_titles,
        )
        .await?;
        pages = pages.saturating_add(p);
        accounts += 1;
        books.extend(batch);
    }

    if accounts == 0 {
        return Err(AudibleError::NoAccounts(
            "no matching Audible accounts in host credentials".into(),
        ));
    }
    Ok(scan_summary(books, accounts, pages))
}

/// Deregister the Audible device that login registered for these credentials.
///
/// Only this device is removed (`deregister_all = false`); the account's other
/// devices stay. Amazon invalidates the tokens on success, so the host deletes
/// the credentials afterwards either way.
///
/// # Arguments
///
/// * `credentials` - Credential JSON exactly as login returned it.
///
/// # Returns
///
/// The device name Amazon assigned at registration, when the credentials
/// record one.
///
/// # Errors
///
/// Returns an error when the credentials do not parse, the token refresh
/// fails, or Amazon rejects the deregistration.
pub async fn guest_revoke_account(credentials: &Value) -> Result<Option<String>> {
    let auth = authenticator_from_credentials(credentials)?;
    let client = Client::new(auth).map_err(AudibleError::from)?;
    deregister_device(&client).await
}

/// Deregister the device behind `client`, leaving the account's other
/// devices registered.
///
/// Exposed for wiremock CI tests (inject a [`Client`] with
/// `auth_base_override`).
///
/// # Returns
///
/// The device name Amazon assigned at registration, when known.
///
/// # Errors
///
/// Returns an error when the token refresh fails or Amazon rejects the
/// deregistration.
pub async fn deregister_device(client: &Client) -> Result<Option<String>> {
    Ok(client.deregister(false).await?)
}

/// Download + decrypt one title; return plain paths (never encrypted on the wire).
///
/// `fetch` carries the host fetch knobs from the ABI `FetchTitleParams.fetch`
/// field, and the instance's `bitrate` setting in `source_config` picks the
/// license tier. Packaging stays with the host.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn guest_fetch_title(
    credentials: &Value,
    title_id: &str,
    cache_dir: &Path,
    source_config: &Value,
    fetch: &FetchOptions,
) -> Result<PlainFetch> {
    let auth = authenticator_from_credentials(credentials)?;
    let marketplace = auth.locale().country_code.to_string();
    let account_id = auth
        .customer_id()
        .map(str::to_string)
        .unwrap_or_else(|| "audible".into());
    let client = Client::new(auth).map_err(AudibleError::from)?;
    let account_client = AccountClient {
        client: std::sync::Arc::new(client),
        account_id,
        marketplace: marketplace.clone(),
    };

    // Optional BYO Widevine CDM from credential blob → cache_dir/widevine.wvd.
    if let Some(wvd_b64) = credentials.get("widevine_b64").and_then(Value::as_str) {
        match STANDARD.decode(wvd_b64) {
            Ok(bytes) => {
                let dest = join_cache_component(cache_dir, "widevine.wvd")
                    .map_err(|e| AudibleError::Other(anyhow::anyhow!("{e}")))?;
                if let Err(err) = tokio::fs::write(&dest, &bytes).await {
                    tracing::warn!(error = %err, "failed to write guest widevine.wvd");
                }
            }
            Err(err) => tracing::warn!(error = %err, "invalid widevine_b64 in credentials"),
        }
    }

    let options = DownloadOptions::from_host(fetch, source_config);

    let (account, downloaded, _summary) =
        fetch_and_download_with_client(account_client, cache_dir, title_id, &options, cache_dir)
            .await?;

    let work_dir = join_cache_component(cache_dir, title_id)
        .map_err(|e| AudibleError::Other(anyhow::anyhow!("{e}")))?;
    tokio::fs::create_dir_all(&work_dir).await?;
    let (chapter_info, cover_path) =
        fetch_companions(&account, title_id, &options, &work_dir).await?;

    let m4b_name = format!("{title_id}.m4b");
    let m4b_path = join_cache_component(&work_dir, &m4b_name)
        .map_err(|e| AudibleError::Other(anyhow::anyhow!("{e}")))?;

    let trim = if options.strip_audible_brand_audio {
        brand_trim(chapter_info.as_ref(), &downloaded.path, title_id).await
    } else {
        None
    };

    let plain_path = if downloaded.needs_decrypt {
        match downloaded.drm_kind {
            DrmKind::Adrm => {
                let key = downloaded.key.clone().ok_or_else(|| {
                    AudibleError::License(format!("{title_id}: Adrm download missing key"))
                })?;
                let iv = downloaded.iv.clone().ok_or_else(|| {
                    AudibleError::License(format!("{title_id}: Adrm download missing iv"))
                })?;
                let outcome = decrypt_adrm(DecryptRequest {
                    input: downloaded.path.clone(),
                    output: m4b_path.clone(),
                    audible_key: Some(key),
                    audible_iv: Some(iv),
                    activation_bytes: None,
                    trim,
                })
                .await
                .map_err(|e| AudibleError::Other(anyhow::anyhow!("decrypt Adrm: {e}")))?;
                outcome.output
            }
            DrmKind::Widevine => {
                let kid = downloaded.kid.clone().ok_or_else(|| {
                    AudibleError::Widevine(format!("{title_id}: Widevine download missing kid"))
                })?;
                let key = downloaded.cenc_key.clone().ok_or_else(|| {
                    AudibleError::Widevine(format!("{title_id}: Widevine download missing key"))
                })?;
                let outcome = decrypt_cenc(CencDecryptRequest {
                    input: downloaded.path.clone(),
                    output: m4b_path.clone(),
                    kid,
                    key,
                    trim,
                })
                .await
                .map_err(|e| AudibleError::Other(anyhow::anyhow!("decrypt CENC: {e}")))?;
                outcome.output
            }
            DrmKind::Mpeg => downloaded.path.clone(),
        }
    } else if downloaded.path != m4b_path {
        if let Some(parent) = m4b_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let from = validated_fs_path(&downloaded.path)
            .map_err(|e| AudibleError::Other(anyhow::anyhow!("{e}")))?;
        tokio::fs::copy(&from, &m4b_path).await?;
        m4b_path
    } else {
        downloaded.path
    };

    let chapters = chapter_info
        .as_ref()
        .map(flatten_chapters)
        .unwrap_or_default();

    Ok(PlainFetch {
        parts: vec![PlainPart {
            path: plain_path.display().to_string(),
            title: None,
            duration_ms: None,
        }],
        m4b_path: Some(plain_path.display().to_string()),
        cover_path: cover_path.map(|p| p.display().to_string()),
        chapters: chapters
            .into_iter()
            .map(|(title, start_ms)| bookclerk_plugin_sdk::ChapterMarker { title, start_ms })
            .collect(),
        pdf_url: downloaded.pdf_url,
    })
}

/// Fetches the chapter metadata and the cover for `title_id`.
///
/// The host re-tags the plain audio, so both are fetched on every fetch. A
/// failed lookup is logged and leaves that piece out rather than failing the
/// fetch.
///
/// # Arguments
///
/// * `account` - Client for the account that owns the title.
/// * `title_id` - Audible ASIN.
/// * `options` - Supplies the bitrate, chapter layout and cover size.
/// * `work_dir` - The title's cache directory; the cover lands here.
///
/// # Returns
///
/// `(chapter_info, cover_path)`, each `None` when its lookup failed or Audible
/// has none.
///
/// # Errors
///
/// Returns an error only when the cover file name cannot be joined to
/// `work_dir` (an invalid `title_id`).
async fn fetch_companions(
    account: &AccountClient,
    title_id: &str,
    options: &DownloadOptions,
    work_dir: &Path,
) -> Result<(Option<Value>, Option<PathBuf>)> {
    let chapter_info = match fetch_chapter_info(
        &account.client,
        &account.marketplace,
        title_id,
        options.quality,
        &options.chapter_layout,
    )
    .await
    {
        Ok(info) => Some(info),
        Err(err) => {
            tracing::warn!(asin = %title_id, error = %err, "guest chapter metadata fetch failed");
            None
        }
    };
    let cover_dest = join_cache_component(work_dir, &format!("{title_id}.cover.jpg"))
        .map_err(|e| AudibleError::Other(anyhow::anyhow!("{e}")))?;
    let cover_path = match download_cover_jpeg(
        &account.client,
        &account.marketplace,
        title_id,
        &options.cover_size,
        &cover_dest,
    )
    .await
    {
        Ok(path) => path,
        Err(err) => {
            tracing::warn!(asin = %title_id, error = %err, "guest cover download failed");
            None
        }
    };
    Ok((chapter_info, cover_path))
}

/// The remux window that drops Audible's brand intro and outro.
///
/// Durations come from `chapter_info`; when an outro is set but the runtime
/// is missing or zero, the runtime is probed from the downloaded media on a
/// blocking thread (the parse reads the whole sample table).
///
/// # Arguments
///
/// * `chapter_info` - Audible chapter metadata, when it was fetched.
/// * `media` - The downloaded (possibly still encrypted) MP4.
/// * `title_id` - Audible ASIN, for the log line.
///
/// # Returns
///
/// The trim window, or `None` when there is no branding or the runtime stays
/// unknown (trimming only the intro would leave the outro in).
async fn brand_trim(
    chapter_info: Option<&Value>,
    media: &Path,
    title_id: &str,
) -> Option<TrimRange> {
    let brand = chapter_info
        .map(brand_durations_from_chapter_info)
        .unwrap_or_default();
    let mut runtime_ms = chapter_info
        .and_then(runtime_length_ms_from_chapter_info)
        .filter(|ms| *ms > 0);
    if brand.outro_ms > 0 && runtime_ms.is_none() {
        let media = media.to_path_buf();
        runtime_ms = tokio::task::spawn_blocking(move || {
            parse_mp4(&media)
                .ok()
                .map(|mp4| track_duration_ms(&mp4.audio))
        })
        .await
        .ok()
        .flatten()
        .filter(|probed| *probed > 0);
    }
    let trim = brand_trim_range(brand, runtime_ms);
    if trim.is_none() && !brand.is_empty() {
        tracing::warn!(
            asin = %title_id,
            intro_ms = brand.intro_ms,
            outro_ms = brand.outro_ms,
            "brand outro set but the runtime is unknown; not trimming, so the \
             outro branding is not left on while only the intro is cut"
        );
    }
    trim
}

/// Build credential JSON from an authenticator (optional Widevine CDM bytes).
///
/// # Errors
///
/// Returns an error when the operation fails.
pub fn credentials_json_from_auth(auth: &Authenticator, widevine: Option<&[u8]>) -> Result<Value> {
    let plain = export_authfile_plain_bytes(auth)?;
    let mut obj = serde_json::Map::new();
    obj.insert("authfile_b64".into(), json!(STANDARD.encode(plain)));
    if let Some(wvd) = widevine {
        obj.insert("widevine_b64".into(), json!(STANDARD.encode(wvd)));
    }
    Ok(Value::Object(obj))
}

/// Rebuilds an [`Authenticator`] from host-unsealed `authfile_b64` JSON (this guest never opens the library DB).
fn authenticator_from_credentials(creds: &Value) -> Result<Authenticator> {
    let b64 = creds
        .get("authfile_b64")
        .and_then(Value::as_str)
        .ok_or_else(|| AudibleError::Auth("credentials missing authfile_b64".into()))?;
    let bytes = STANDARD
        .decode(b64)
        .map_err(|e| AudibleError::Auth(format!("invalid authfile_b64: {e}")))?;
    Authenticator::load_from_bytes(&bytes, None)
        .map_err(|e| AudibleError::Auth(format!("failed to decode audible auth: {e}")))
}

/// Parses the OAuth callback bind address; empty defaults to `127.0.0.1:0`.
fn parse_callback_bind(raw: Option<&str>) -> Result<SocketAddr> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok("127.0.0.1:0".parse().expect("valid socket addr")),
        Some(s) => s
            .parse()
            .map_err(|e| AudibleError::Auth(format!("invalid callback_bind `{s}`: {e}"))),
    }
}

/// Flattens Audible `chapter_info` into `(title, start_ms)` pairs, sorted and deduped by start.
fn flatten_chapters(info: &Value) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    if let Some(arr) = info.get("chapters").and_then(Value::as_array) {
        flatten_chapter_nodes(arr, &mut out);
    }
    out.sort_by_key(|(_, start)| *start);
    out.dedup_by_key(|(_, start)| *start);
    out
}

/// Depth-first walk of nested `chapters` arrays, emitting each node's title and start offset.
fn flatten_chapter_nodes(nodes: &[Value], out: &mut Vec<(String, u64)>) {
    for node in nodes {
        if let Some(nested) = node.get("chapters").and_then(Value::as_array) {
            flatten_chapter_nodes(nested, out);
        }
        let Some(title) = node.get("title").and_then(Value::as_str) else {
            continue;
        };
        let start_ms = node
            .get("start_offset_ms")
            .and_then(Value::as_u64)
            .or_else(|| {
                node.get("start_offset_ms")
                    .and_then(Value::as_i64)
                    .filter(|v| *v >= 0)
                    .map(|v| v as u64)
            })
            .or_else(|| node.get("startOffsetMs").and_then(Value::as_u64))
            .unwrap_or(0);
        if !title.trim().is_empty() {
            out.push((title.trim().to_string(), start_ms));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn account_for(server: &MockServer) -> AccountClient {
        let auth = Authenticator::from_value(json!({
            "country_code": "us",
            "identity": {"customer_id": "amzn1.account.TEST"},
            "bearer": {
                "access_token": "Atna|synthetic",
                "refresh_token": "Atnr|synthetic",
                "expires": 9999999999.0
            }
        }))
        .unwrap();
        let url = url::Url::parse(&server.uri()).unwrap();
        let client = Client::builder(auth)
            .api_base_override(url.clone())
            .auth_base_override(url)
            .build()
            .unwrap();
        AccountClient {
            client: std::sync::Arc::new(client),
            account_id: "amzn1.account.TEST".into(),
            marketplace: "us".into(),
        }
    }

    #[tokio::test]
    async fn companions_keep_the_chapters_when_the_cover_fails() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/1.0/content/B0BOOK0001/metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content_metadata": {"chapter_info": {
                    "brandIntroDurationMs": 2000,
                    "chapters": [{"title": "One", "start_offset_ms": 0}]
                }}
            })))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let (chapters, cover) = fetch_companions(
            &account_for(&server),
            "B0BOOK0001",
            &DownloadOptions::default(),
            dir.path(),
        )
        .await
        .unwrap();
        assert_eq!(
            flatten_chapters(&chapters.unwrap()),
            [("One".to_string(), 0)]
        );
        assert!(cover.is_none());
    }

    #[tokio::test]
    async fn companions_keep_the_cover_when_the_chapters_fail() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/1.0/catalog/products/B0BOOK0001"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "product": {"product_images": {
                    "500": format!("{}/images/B0BOOK0001._SL500_.jpg", server.uri())
                }}
            })))
            .mount(&server)
            .await;
        let jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F'];
        Mock::given(method("GET"))
            .and(path("/images/B0BOOK0001._SL500_.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "image/jpeg")
                    .set_body_bytes(jpeg.clone()),
            )
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let (chapters, cover) = fetch_companions(
            &account_for(&server),
            "B0BOOK0001",
            &DownloadOptions::default(),
            dir.path(),
        )
        .await
        .unwrap();
        assert!(chapters.is_none());
        let cover = cover.expect("cover downloaded");
        assert_eq!(cover, dir.path().join("B0BOOK0001.cover.jpg"));
        assert_eq!(std::fs::read(&cover).unwrap(), jpeg);
    }

    #[tokio::test]
    async fn companions_are_dropped_when_audible_has_none() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let (chapters, cover) = fetch_companions(
            &account_for(&server),
            "B0BOOK0001",
            &DownloadOptions::default(),
            dir.path(),
        )
        .await
        .unwrap();
        assert!(chapters.is_none());
        assert!(cover.is_none());
    }

    /// The trim window as `(start, end)` for `info` against `media`.
    async fn window(info: Option<&Value>, media: &Path) -> Option<(u64, Option<u64>)> {
        brand_trim(info, media, "B0BOOK0001")
            .await
            .map(|t| (t.start_ms, t.end_ms))
    }

    #[tokio::test]
    async fn brand_trim_cuts_the_intro_and_needs_a_runtime_for_the_outro() {
        let missing = Path::new("/nonexistent/download.aaxc");
        assert_eq!(window(None, missing).await, None);
        assert_eq!(
            window(Some(&json!({"brand_intro_duration_ms": 4000})), missing).await,
            Some((4000, None))
        );
        let outro = json!({"brand_intro_duration_ms": 4000, "brand_outro_duration_ms": 3000});
        assert_eq!(window(Some(&outro), missing).await, None);
        let with_runtime = json!({
            "brand_intro_duration_ms": 4000,
            "brand_outro_duration_ms": 3000,
            "runtime_length_ms": 60000
        });
        assert_eq!(
            window(Some(&with_runtime), missing).await,
            Some((4000, Some(57000)))
        );
    }

    #[tokio::test]
    async fn brand_trim_probes_the_media_when_the_runtime_is_missing_or_zero() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("download.m4b");
        let fixture = bookclerk_mp4::fixture::ProgressiveFixture::with_generated_samples(87);
        fixture.write(&media).unwrap();
        let runtime = fixture.duration_ms();
        assert!(runtime > 500, "fixture runtime {runtime} ms");
        let expected = Some((0, Some(runtime - 500)));
        let no_runtime = json!({"brand_outro_duration_ms": 500});
        assert_eq!(window(Some(&no_runtime), &media).await, expected);
        let zero_runtime = json!({"brand_outro_duration_ms": 500, "runtime_length_ms": 0});
        assert_eq!(window(Some(&zero_runtime), &media).await, expected);
    }

    /// GET `url` over a bare HTTP/1.1 connection and return the raw response.
    async fn http_get(url: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let url = url::Url::parse(url).unwrap();
        let addr = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap());
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            url.path(),
            url.host_str().unwrap()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        String::from_utf8_lossy(&response).into_owned()
    }

    #[tokio::test]
    async fn login_start_serves_a_local_landing_page_and_tracks_the_session() {
        let params = LoginParams {
            marketplace: " UK ".into(),
            label: Some("Main".into()),
            callback_bind: Some("127.0.0.1:0".into()),
            timeout_secs: Some(5),
            ..LoginParams::default()
        };
        let (session_id, url) = guest_login_start(&params).await.unwrap();
        assert!(url.starts_with("http://127.0.0.1:"), "{url}");
        let page = http_get(&url).await;
        assert!(page.starts_with("HTTP/1.1 200"), "{page}");
        assert!(page.contains("/__start"), "{page}");
        let pending = pending_logins()
            .lock()
            .unwrap()
            .remove(&session_id)
            .unwrap();
        assert_eq!(pending.label.as_deref(), Some("Main"));
        pending.handle.abort();
        assert!(guest_login_complete(&session_id).await.is_err());
    }
}
