//! [`ContentSource`] adapter for Audible.

use std::path::Path;
use std::sync::Arc;

use crate::drm::{
    decrypt_adrm, decrypt_cenc, CencDecryptRequest, DecryptRequest, TrimRange as DrmTrimRange,
};
use async_trait::async_trait;
use bookclerk_config::{AudioQuality, Config};
use bookclerk_library::{secret_kind, SourceScope};
use bookclerk_media::{
    brand_durations_from_chapter_info, brand_trim_range, parse_mp4,
    runtime_length_ms_from_chapter_info, track_duration_ms,
};
use bookclerk_source::{
    revoke_credentials_default, CatalogHit, CatalogSearchOpts, ContentSource, ExpandSeed,
    FetchOptions, ImportCredentialsOptions, LoginOptions, OAuthProgress, PlainAudioPart,
    PlainFetch, PortalAuthMode, PurchaseHintOpts, Result, ScanOptions, ScanSummary, SourceAccount,
    SourceBrand, SourceError, SourceFetch, SourcePurchaseHint, SourceRegistry,
};
use serde_json::Value;

use crate::accounts::{import_auth_file, import_libation_accounts_json, import_mkb79_auth_json};
use crate::artifacts::{download_cover_jpeg, fetch_chapter_info};
use crate::auth::{begin_login, AuthLoginOptions, LoginMode};
use crate::db::{delete_audible_account_from_db, list_audible_accounts_from_db};
use crate::download::{
    fetch_and_download_with_options, open_account_client, request_content_license,
    summarize_license, DrmKind,
};
use crate::error::AudibleError;
use crate::qr::QrRenderMode;
use crate::sync::scan_library;

/// Canonical plugin id.
pub const ID: &str = "audible";

/// Audible store as a [`ContentSource`].
#[derive(Debug, Default, Clone, Copy)]
pub struct AudibleSource {
    /// License bitrate tier (`[sources.audible] bitrate`).
    pub bitrate: AudioQuality,
}

impl AudibleSource {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse `[sources.audible]` knobs from config.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let bitrate = config
            .sources
            .get_string(ID, "bitrate")
            .and_then(parse_audio_quality)
            .unwrap_or_default();
        Self { bitrate }
    }

    #[must_use]
    pub fn with_bitrate(mut self, bitrate: AudioQuality) -> Self {
        self.bitrate = bitrate;
        self
    }
}

#[async_trait]
impl ContentSource for AudibleSource {
    fn id(&self) -> &str {
        ID
    }

    fn display_name(&self) -> &str {
        "Audible"
    }

    fn portal_auth_mode(&self) -> PortalAuthMode {
        PortalAuthMode::Oauth
    }

    fn portal_brand(&self) -> SourceBrand {
        SourceBrand {
            id: "audible",
            name: "Audible",
            bg: "#F8991D",
            fg: "#111111",
            accent: "#D97706",
            icon_url: "https://www.google.com/s2/favicons?domain=audible.com&sz=128",
        }
    }

    fn sort_key(&self) -> u32 {
        0
    }

    async fn login(&self, scope: &SourceScope, opts: LoginOptions) -> Result<SourceAccount> {
        self.login_with_oauth_progress(scope, opts, &|_| {}).await
    }

    async fn login_with_oauth_progress(
        &self,
        scope: &SourceScope,
        opts: LoginOptions,
        on_progress: &(dyn Fn(OAuthProgress) + Send + Sync),
    ) -> Result<SourceAccount> {
        // Audible ignores email/password (OAuth via browser/QR). Drive the
        // interactive LoginServer flow with marketplace / label / force.
        let marketplace = if opts.marketplace.trim().is_empty() {
            String::from("us")
        } else {
            opts.marketplace
        };
        let callback_bind = opts
            .callback_bind
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| "127.0.0.1:0".parse().expect("valid socket addr"));
        let audible_username = opts
            .extra
            .get("audible_username")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let ascii_qr = opts
            .extra
            .get("ascii_qr")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mode = if opts.external || opts.response_url.is_some() {
            LoginMode::External
        } else {
            LoginMode::Server
        };
        let auth_opts = AuthLoginOptions {
            marketplace,
            label: opts.label,
            mode,
            force: opts.force,
            callback_bind,
            response_url: opts.response_url,
            show_qr: opts.show_qr,
            qr_mode: if ascii_qr {
                QrRenderMode::Ascii
            } else {
                QrRenderMode::Unicode
            },
            timeout_secs: opts.timeout_secs.unwrap_or(300),
            audible_username,
            scope: Some(scope.clone()),
        };
        let session = begin_login(auth_opts, |progress| match progress {
            crate::auth::LoginProgress::LoginUrl { url, qr } => {
                on_progress(OAuthProgress::LoginUrl { url, qr });
            }
            crate::auth::LoginProgress::CallbackListening { addr } => {
                on_progress(OAuthProgress::CallbackListening {
                    addr: addr.to_string(),
                });
            }
            crate::auth::LoginProgress::WaitingForCallback => {
                on_progress(OAuthProgress::WaitingForCallback);
            }
            crate::auth::LoginProgress::Completed { account_id } => {
                on_progress(OAuthProgress::Completed { account_id });
            }
        })
        .await
        .map_err(map_audible_err)?;
        Ok(SourceAccount {
            account_id: session.account_id,
            source: ID.into(),
            marketplace: session.marketplace,
            label: session.label,
            scan_enabled: true,
        })
    }

    async fn import_credentials(
        &self,
        scope: &SourceScope,
        path: &Path,
        opts: ImportCredentialsOptions,
    ) -> Result<Vec<SourceAccount>> {
        if opts.libation_accounts || path.ends_with("AccountsSettings.json") {
            let accounts = import_libation_accounts_json(path).map_err(map_audible_err)?;
            let mut out = Vec::with_capacity(accounts.len());
            for acct in accounts {
                scope
                    .upsert_account(
                        &acct.account_id,
                        &acct.marketplace,
                        acct.label.as_deref(),
                        true,
                    )
                    .await
                    .map_err(|e| SourceError::api(e.to_string()))?;
                out.push(SourceAccount {
                    account_id: acct.account_id,
                    source: ID.into(),
                    marketplace: acct.marketplace,
                    label: acct.label,
                    scan_enabled: true,
                });
            }
            return Ok(out);
        }

        let acct = if opts.mkb79 {
            import_mkb79_auth_json(scope, path, opts.label.as_deref(), opts.force)
                .await
                .map_err(map_audible_err)?
        } else {
            import_auth_file(scope, path, opts.label.as_deref(), opts.force)
                .await
                .map_err(map_audible_err)?
        };
        scope
            .upsert_account(
                &acct.account_id,
                &acct.marketplace,
                acct.label.as_deref(),
                true,
            )
            .await
            .map_err(|e| SourceError::api(e.to_string()))?;
        Ok(vec![SourceAccount {
            account_id: acct.account_id,
            source: ID.into(),
            marketplace: acct.marketplace,
            label: acct.label,
            scan_enabled: true,
        }])
    }

    async fn revoke_credentials(&self, scope: &SourceScope, account_id: &str) -> Result<()> {
        if let Err(e) = delete_audible_account_from_db(scope, account_id).await {
            tracing::warn!(
                error = %e,
                account = %account_id,
                "failed to delete audible auth secret"
            );
        }
        let wvd = format!("{account_id}.wvd");
        if let Err(e) = scope
            .delete_secret(secret_kind::WIDEVINE, account_id, &wvd)
            .await
        {
            tracing::warn!(
                error = %e,
                account = %account_id,
                "failed to delete widevine cdm"
            );
        }
        revoke_credentials_default(scope, account_id).await
    }

    async fn list_accounts(&self, scope: &SourceScope) -> Result<Vec<SourceAccount>> {
        // Prefer the accounts table (customer id + display label). Fall back to
        // secret rows when an account has credentials but no metadata row yet.
        let mut by_id: std::collections::BTreeMap<String, SourceAccount> = scope
            .list_accounts()
            .await
            .map_err(|e| SourceError::api(e.to_string()))?
            .into_iter()
            .map(|a| {
                (
                    a.account_id.clone(),
                    SourceAccount {
                        account_id: a.account_id,
                        source: ID.into(),
                        marketplace: a.marketplace,
                        label: a.label,
                        scan_enabled: a.scan_enabled,
                    },
                )
            })
            .collect();
        let secrets = list_audible_accounts_from_db(scope)
            .await
            .map_err(map_audible_err)?;
        for (account_id, _name) in secrets {
            by_id.entry(account_id.clone()).or_insert(SourceAccount {
                account_id,
                source: ID.into(),
                marketplace: String::new(),
                label: None,
                scan_enabled: true,
            });
        }
        Ok(by_id.into_values().collect())
    }

    async fn scan(&self, scope: &SourceScope, opts: ScanOptions) -> Result<ScanSummary> {
        scan_library(scope, opts).await.map_err(map_audible_err)
    }

    async fn fetch_title(
        &self,
        scope: &SourceScope,
        account_id: &str,
        title_id: &str,
        opts: &FetchOptions,
    ) -> Result<SourceFetch> {
        let mut dl = opts.download.clone();
        dl.quality = self.bitrate;

        let (account, download, _summary) = fetch_and_download_with_options(
            scope,
            &opts.files_dir,
            account_id,
            title_id,
            &dl,
            &opts.cache_dir,
        )
        .await
        .map_err(map_audible_err)?;

        let need_chapters = dl.create_cue
            || dl.fixup_metadata
            || dl.wants_chapter_json()
            || dl.wants_split_by_chapter()
            || dl.strip_audible_brand_audio;
        let mut chapter_info = None;
        if need_chapters {
            match fetch_chapter_info(
                &account.client,
                &account.marketplace,
                title_id,
                dl.quality,
                &dl.chapter_layout,
            )
            .await
            {
                Ok(info) => chapter_info = Some(info),
                Err(err) => {
                    tracing::warn!(asin = %title_id, error = %err, "chapter metadata fetch failed");
                }
            }
        }

        let want_cover = dl.download_cover || dl.fixup_metadata;
        let mut cover_path = None;
        if want_cover {
            let dest = opts.cache_dir.join(format!("{title_id}.cover.jpg"));
            match download_cover_jpeg(
                &account.client,
                &account.marketplace,
                title_id,
                &dl.cover_size,
                &dest,
            )
            .await
            {
                Ok(path) => cover_path = path,
                Err(err) => {
                    tracing::warn!(asin = %title_id, error = %err, "cover download failed");
                }
            }
        }

        let work_dir = opts.cache_dir.join(title_id);
        tokio::fs::create_dir_all(&work_dir)
            .await
            .map_err(SourceError::Io)?;
        let m4b_path = work_dir.join(format!("{title_id}.m4b"));

        let trim = if dl.strip_audible_brand_audio {
            let brand = chapter_info
                .as_ref()
                .map(brand_durations_from_chapter_info)
                .unwrap_or_default();
            let mut runtime_ms = chapter_info
                .as_ref()
                .and_then(runtime_length_ms_from_chapter_info);
            if brand.outro_ms > 0 && runtime_ms.is_none() {
                if let Ok(mp4) = parse_mp4(&download.path) {
                    let probed = track_duration_ms(&mp4.audio);
                    if probed > 0 {
                        runtime_ms = Some(probed);
                    }
                }
            }
            brand_trim_range(brand, runtime_ms).map(|t| DrmTrimRange {
                start_ms: t.start_ms,
                end_ms: t.end_ms,
            })
        } else {
            None
        };

        let plain_path = if download.needs_decrypt {
            match download.drm_kind {
                DrmKind::Adrm => {
                    let key = download.key.ok_or_else(|| {
                        SourceError::api(format!("{title_id}: Adrm download missing key"))
                    })?;
                    let iv = download.iv.ok_or_else(|| {
                        SourceError::api(format!("{title_id}: Adrm download missing iv"))
                    })?;
                    let outcome = decrypt_adrm(DecryptRequest {
                        input: download.path.clone(),
                        output: m4b_path.clone(),
                        audible_key: Some(key),
                        audible_iv: Some(iv),
                        activation_bytes: None,
                        trim,
                    })
                    .await
                    .map_err(|e| SourceError::api(format!("decrypt Adrm: {e}")))?;
                    outcome.output
                }
                DrmKind::Widevine => {
                    let kid = download.kid.ok_or_else(|| {
                        SourceError::api(format!("{title_id}: Widevine download missing kid"))
                    })?;
                    let key = download.cenc_key.ok_or_else(|| {
                        SourceError::api(format!("{title_id}: Widevine download missing key"))
                    })?;
                    let outcome = decrypt_cenc(CencDecryptRequest {
                        input: download.path.clone(),
                        output: m4b_path.clone(),
                        kid,
                        key,
                        trim,
                    })
                    .await
                    .map_err(|e| SourceError::api(format!("decrypt CENC: {e}")))?;
                    outcome.output
                }
                DrmKind::Mpeg => download.path.clone(),
            }
        } else if download.path != m4b_path {
            tokio::fs::copy(&download.path, &m4b_path)
                .await
                .map_err(SourceError::Io)?;
            m4b_path
        } else {
            download.path
        };

        let chapters = chapter_info
            .as_ref()
            .map(flatten_chapters)
            .unwrap_or_default();

        Ok(PlainFetch {
            parts: vec![PlainAudioPart {
                path: plain_path.clone(),
                title: None,
                duration_ms: None,
            }],
            m4b_path: Some(plain_path),
            cover_path,
            chapters,
            pdf_url: download.pdf_url,
        })
    }

    fn config_options(&self) -> &'static [bookclerk_source::SourceConfigOption] {
        AUDIBLE_CONFIG_OPTIONS
    }

    async fn search_catalog(&self, opts: &CatalogSearchOpts) -> Result<Vec<CatalogHit>> {
        crate::catalog::search_catalog(opts).await
    }

    async fn expand_candidates(&self, seed: &ExpandSeed, limit: usize) -> Result<Vec<CatalogHit>> {
        crate::catalog::expand_candidates(seed, limit).await
    }

    async fn purchase_hint(&self, opts: &PurchaseHintOpts) -> Result<Option<SourcePurchaseHint>> {
        crate::catalog::purchase_hint(opts).await
    }

    async fn inspect_title(
        &self,
        scope: &SourceScope,
        account_id: &str,
        title_id: &str,
        opts: &FetchOptions,
    ) -> Result<serde_json::Value> {
        let mut quality = self.bitrate;
        if opts.download.quality != quality {
            quality = opts.download.quality;
        }
        let client = open_account_client(scope, account_id)
            .await
            .map_err(map_audible_err)?;
        let license =
            request_content_license(&client.client, &client.marketplace, title_id, quality)
                .await
                .map_err(map_audible_err)?;
        let summary = summarize_license(&license);
        Ok(serde_json::json!({
            "summary": summary,
            "full": license.raw,
        }))
    }
}

const AUDIBLE_CONFIG_OPTIONS: &[bookclerk_source::SourceConfigOption] =
    &[bookclerk_source::SourceConfigOption {
        key: "bitrate",
        label: "Bitrate",
        values: &[
            bookclerk_source::ConfigOptionValue {
                id: "high",
                label: "High",
            },
            bookclerk_source::ConfigOptionValue {
                id: "normal",
                label: "Normal",
            },
        ],
    }];

fn parse_audio_quality(raw: &str) -> Option<AudioQuality> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "high" => Some(AudioQuality::High),
        "normal" => Some(AudioQuality::Normal),
        _ => None,
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

fn map_audible_err(err: AudibleError) -> SourceError {
    match err {
        AudibleError::NoAccounts(msg) => SourceError::NoAccounts(msg),
        AudibleError::Auth(msg) | AudibleError::AccountNotFound(msg) => SourceError::Auth(msg),
        AudibleError::Io(err) => SourceError::Io(err),
        AudibleError::Other(err) => SourceError::Other(err),
        other => SourceError::Api(other.to_string()),
    }
}

/// Parse `[sources.audible]` into an [`AudibleSource`].
#[must_use]
pub fn from_config(config: &Config) -> AudibleSource {
    AudibleSource::from_config(config)
}

/// Register Audible when `[sources.audible] enabled` (default true).
pub fn register(registry: &mut SourceRegistry, config: &Config) {
    if config.sources.is_enabled(ID) {
        registry.register(Arc::new(from_config(config)));
    }
}
