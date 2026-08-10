//! Guest-process helpers for the external Audible source plugin.
//!
//! Credentials are opaque JSON (`authfile_b64`, optional `widevine_b64`). The
//! host seals them; this module never opens the library DB.
//!
//! Note: audible-rs opens HTTP sockets directly under coarse jail outbound
//! (`NetPolicy::Outbound` / `OutboundListen` with oauth). Native plugins omit
//! `capabilities.network.domains` (hostname allowlists are workerd-only).

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::drm::{decrypt_adrm, decrypt_cenc, CencDecryptRequest, DecryptRequest};
use audible_rs::api::client::Client;
use audible_rs::auth::login::{self as login_flow, LoginServer};
use audible_rs::auth::Authenticator;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use bookclerk_config::AudioQuality;
use bookclerk_media::{
    brand_durations_from_chapter_info, brand_trim_range, parse_mp4,
    runtime_length_ms_from_chapter_info, track_duration_ms,
};
use bookclerk_plugin_sdk::{
    CatalogHitDto, LoginParams, LoginResultDto, PlainPartDto, PurchaseHintDto, ScanBookDto,
    ScanSummaryDto, SourceAccountDto, SourceFetchDto,
};
use serde_json::{json, Value};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::artifacts::{download_cover_jpeg, fetch_chapter_info};
use crate::auth::{
    export_authfile_plain_bytes, login_defaults, login_server_url, session_from_authenticator,
    AuthLoginOptions,
};
use crate::download::{fetch_and_download_with_client, AccountClient, DrmKind};
use crate::error::{AudibleError, Result};
use crate::options::DownloadOptions;
use crate::source::ID;
use crate::sync::collect_account_books;

struct PendingGuestLogin {
    label: Option<String>,
    handle: JoinHandle<Result<Authenticator>>,
}

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
pub async fn guest_login_start(params: &LoginParams) -> Result<(String, String)> {
    let marketplace = if params.marketplace.trim().is_empty() {
        String::from("us")
    } else {
        params.marketplace.trim().to_ascii_lowercase()
    };
    let callback_bind = parse_callback_bind(params.callback_bind.as_deref())?;
    let opts = AuthLoginOptions {
        marketplace,
        label: params.label.clone(),
        callback_bind,
        show_qr: false,
        scope: None,
        force: params.force,
        timeout_secs: params.timeout_secs.unwrap_or(300),
        ..AuthLoginOptions::default()
    };

    let defaults = login_defaults(&opts);
    let timeout = Duration::from_secs(opts.timeout_secs);
    let label = opts.label.clone();

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
        let server = LoginServer::bind(opts.callback_bind, defaults).await?;
        let (url, _addr) = login_server_url(&server, opts.callback_bind);
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

async fn register_after_login(
    login: audible_rs::auth::login::ServerLogin,
) -> Result<Authenticator> {
    let http = reqwest::Client::builder()
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

/// Await a pending OAuth session and return account + credential JSON.
pub async fn guest_login_complete(session_id: &str) -> Result<LoginResultDto> {
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

    Ok(LoginResultDto {
        account: SourceAccountDto {
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
pub async fn guest_scan(
    credentials: &BTreeMap<String, Value>,
    account_filter: &[String],
    page_size: u32,
    import_episodes: bool,
    import_plus_titles: bool,
) -> Result<ScanSummaryDto> {
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
        books.extend(batch.into_iter().map(new_book_to_scan));
    }

    if accounts == 0 {
        return Err(AudibleError::NoAccounts(
            "no matching Audible accounts in host credentials".into(),
        ));
    }
    let n = books.len();
    Ok(ScanSummaryDto {
        accounts,
        books_upserted: n,
        pages,
        skipped_disabled: 0,
        books,
    })
}

/// Download + decrypt one title; return plain paths (never encrypted on the wire).
///
/// `download` is the host [`DownloadOptions`] JSON from [`FetchTitleParams::download`].
/// Plugin bitrate from `source_config` overlays `quality` (same as in-process).
pub async fn guest_fetch_title(
    credentials: &Value,
    title_id: &str,
    cache_dir: &Path,
    source_config: &Value,
    download: &Value,
) -> Result<SourceFetchDto> {
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
                let dest = cache_dir.join("widevine.wvd");
                if let Err(err) = tokio::fs::write(&dest, &bytes).await {
                    tracing::warn!(error = %err, "failed to write guest widevine.wvd");
                }
            }
            Err(err) => tracing::warn!(error = %err, "invalid widevine_b64 in credentials"),
        }
    }

    let options = download_options_from_host(download, source_config);

    let (account, downloaded, _summary) = fetch_and_download_with_client(
        account_client,
        cache_dir,
        title_id,
        &options,
        cache_dir,
        None,
    )
    .await?;

    let need_chapters = options.create_cue
        || options.fixup_metadata
        || options.wants_chapter_json()
        || options.wants_split_by_chapter()
        || options.strip_audible_brand_audio;
    let mut chapter_info = None;
    if need_chapters {
        match fetch_chapter_info(
            &account.client,
            &account.marketplace,
            title_id,
            options.quality,
            &options.chapter_layout,
        )
        .await
        {
            Ok(info) => chapter_info = Some(info),
            Err(err) => {
                tracing::warn!(asin = %title_id, error = %err, "guest chapter metadata fetch failed");
            }
        }
    }

    let want_cover = options.download_cover || options.fixup_metadata;
    let mut cover_path = None;
    if want_cover {
        let work_dir = cache_dir.join(title_id);
        tokio::fs::create_dir_all(&work_dir).await?;
        let cover_dest = work_dir.join(format!("{title_id}.cover.jpg"));
        match download_cover_jpeg(
            &account.client,
            &account.marketplace,
            title_id,
            &options.cover_size,
            &cover_dest,
        )
        .await
        {
            Ok(path) => cover_path = path,
            Err(err) => {
                tracing::warn!(asin = %title_id, error = %err, "guest cover download failed");
            }
        }
    }

    let work_dir = cache_dir.join(title_id);
    tokio::fs::create_dir_all(&work_dir).await?;
    let m4b_path = work_dir.join(format!("{title_id}.m4b"));

    let trim = if options.strip_audible_brand_audio {
        let brand = chapter_info
            .as_ref()
            .map(brand_durations_from_chapter_info)
            .unwrap_or_default();
        let mut runtime_ms = chapter_info
            .as_ref()
            .and_then(runtime_length_ms_from_chapter_info);
        if brand.outro_ms > 0 && runtime_ms.is_none() {
            if let Ok(mp4) = parse_mp4(&downloaded.path) {
                let probed = track_duration_ms(&mp4.audio);
                if probed > 0 {
                    runtime_ms = Some(probed);
                }
            }
        }
        brand_trim_range(brand, runtime_ms)
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
        tokio::fs::copy(&downloaded.path, &m4b_path).await?;
        m4b_path
    } else {
        downloaded.path
    };

    let chapters = chapter_info
        .as_ref()
        .map(flatten_chapters)
        .unwrap_or_default();

    Ok(SourceFetchDto::Plain {
        parts: vec![PlainPartDto {
            path: plain_path.display().to_string(),
            title: None,
            duration_ms: None,
        }],
        m4b_path: Some(plain_path.display().to_string()),
        cover_path: cover_path.map(|p| p.display().to_string()),
        chapters,
        pdf_url: downloaded.pdf_url,
    })
}

/// Merge host download options with `[sources.audible]` bitrate (in-process parity).
fn download_options_from_host(download: &Value, source_config: &Value) -> DownloadOptions {
    let mut options = if download.is_null() {
        DownloadOptions::default()
    } else {
        serde_json::from_value(download.clone()).unwrap_or_else(|err| {
            tracing::warn!(error = %err, "invalid fetch_title download options; using defaults");
            DownloadOptions::default()
        })
    };
    options.quality = resolve_bitrate(source_config);
    options
}

/// Build credential JSON from an authenticator (optional Widevine CDM bytes).
pub fn credentials_json_from_auth(auth: &Authenticator, widevine: Option<&[u8]>) -> Result<Value> {
    let plain = export_authfile_plain_bytes(auth)?;
    let mut obj = serde_json::Map::new();
    obj.insert("authfile_b64".into(), json!(STANDARD.encode(plain)));
    if let Some(wvd) = widevine {
        obj.insert("widevine_b64".into(), json!(STANDARD.encode(wvd)));
    }
    Ok(Value::Object(obj))
}

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

fn parse_callback_bind(raw: Option<&str>) -> Result<SocketAddr> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok("127.0.0.1:0".parse().expect("valid socket addr")),
        Some(s) => s
            .parse()
            .map_err(|e| AudibleError::Auth(format!("invalid callback_bind `{s}`: {e}"))),
    }
}

fn resolve_bitrate(source_config: &Value) -> AudioQuality {
    source_config
        .get("bitrate")
        .and_then(Value::as_str)
        .and_then(|raw| match raw.trim().to_ascii_lowercase().as_str() {
            "high" => Some(AudioQuality::High),
            "normal" => Some(AudioQuality::Normal),
            _ => None,
        })
        .unwrap_or_default()
}

fn new_book_to_scan(book: bookclerk_library::NewBook) -> ScanBookDto {
    ScanBookDto {
        account_id: book.account_id,
        product_id: book.product_id,
        title: book.title,
        marketplace: Some(book.marketplace),
        asin: book.asin,
        isbn: book.isbn,
        authors: book.authors,
        narrators: book.narrators,
        series: book.series,
        series_index: book.series_index,
        content_kind: Some(book.content_kind),
        publisher: book.publisher,
        length_minutes: book.length_minutes,
        subtitle: book.subtitle,
    }
}

fn flatten_chapters(info: &Value) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    if let Some(arr) = info.get("chapters").and_then(Value::as_array) {
        flatten_chapter_nodes(arr, &mut out);
    }
    out.sort_by_key(|(_, start)| *start);
    out.dedup_by_key(|(_, start)| *start);
    out
}

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

/// Map a catalog hit to the plugin-protocol DTO.
#[must_use]
pub fn catalog_hit_to_dto(hit: bookclerk_source::CatalogHit) -> CatalogHitDto {
    CatalogHitDto {
        product_id: hit.product_id,
        title: hit.title,
        authors: hit.authors,
        narrators: hit.narrators,
        series: hit.series,
        series_index: hit.series_index,
        asin: hit.asin,
        isbn: hit.isbn,
        url: hit.url,
        cover_url: hit.cover_url,
        origin: hit.origin,
        subtitle: hit.subtitle,
        description: hit.description,
        publisher: hit.publisher,
        length_minutes: hit.length_minutes,
        published_at: hit.published_at,
        categories: hit.categories,
        language: hit.language,
        price_cents: hit.price_cents,
        currency: hit.currency,
        price_label: hit.price_label,
        rating_overall: hit.rating_overall,
        rating_count: hit.rating_count,
        is_abridged: hit.is_abridged,
    }
}

/// Map a purchase hint to the plugin-protocol DTO.
#[must_use]
pub fn purchase_hint_to_dto(hint: bookclerk_source::SourcePurchaseHint) -> PurchaseHintDto {
    PurchaseHintDto {
        product_id: hint.product_id,
        title: hint.title,
        url: hint.url,
        price_cents: hint.price_cents,
        currency: hint.currency,
        price_label: hint.price_label,
        list_price_cents: hint.list_price_cents,
        list_price_label: hint.list_price_label,
        member_price_cents: hint.member_price_cents,
        member_price_label: hint.member_price_label,
    }
}
